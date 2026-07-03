//! Surface pump: owns the wgpu surface and every swapchain call, on a
//! dedicated thread on Windows and inline elsewhere.
//!
//! On Windows the editor frame loop runs on the host's GUI thread, and
//! any wgpu call that enters the graphics driver can park that thread
//! for an unbounded time - a stalled AMD driver was measured blocking
//! device creation, swapchain reconfigure (`DestroyAllocation2`), and
//! acquire in kernel calls that never return, freezing the DAW. The
//! threaded pump moves all of those off the GUI thread:
//!
//! - **init**: instance / surface / adapter / device creation runs on
//!   the pump thread; the GUI adopts the product when it lands.
//! - **configure**: resizes are queued latest-wins and applied there.
//! - **acquire**: the pump pre-acquires the next frame, so the GUI
//!   thread picks up an already-acquired texture or skips the paint.
//! - **present**: painted frames are handed back and presented there.
//!
//! The GUI thread's only remaining GPU work is encoding + submitting
//! into a pre-acquired texture - queue submission was never observed
//! blocking in any captured hang.
//!
//! On macOS / Linux the same [`PumpClient`] API runs everything
//! synchronously on the calling thread (their drivers don't exhibit
//! the unbounded blocking, and macOS ties layer updates to the main
//! thread), so editors share one code path across platforms.

#[cfg(target_os = "windows")]
use std::sync::Condvar;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// What the per-backend init closure returns: the GUI-side product
/// (device handles, pipelines, ...) plus the device + configuration
/// the pump needs for `surface.configure`.
pub type PumpInit<T> = (T, wgpu::Device, wgpu::SurfaceConfiguration);

/// Per-backend GPU init, run once the instance / adapter / surface
/// exist (on the pump thread on Windows, inline elsewhere). Returns
/// `None` on failure (editor stays blank, host survives). Must NOT
/// configure the surface - the pump does that with the returned
/// configuration.
pub type PumpInitFn<T> = Box<
    dyn FnOnce(&wgpu::Instance, &wgpu::Adapter, &wgpu::Surface<'static>) -> Option<PumpInit<T>>
        + Send,
>;

// --- Windows: threaded pump ---

/// GPU init still running on the pump thread.
#[cfg(target_os = "windows")]
const STATE_INIT: u8 = 0;
/// Init finished; the pump is serving frames.
#[cfg(target_os = "windows")]
const STATE_READY: u8 = 1;
/// Init failed or the pump thread died; the editor stays blank until
/// a device-loss recovery spawns a fresh pump.
#[cfg(target_os = "windows")]
const STATE_FAILED: u8 = 2;

/// Latest-wins mailbox between the GUI thread and the pump thread.
// The bools are independent protocol flags (want / taken / shutdown /
// exited), not a state machine in disguise; an enum would obscure
// which combinations are legal.
#[allow(clippy::struct_excessive_bools)]
#[cfg(target_os = "windows")]
#[derive(Default)]
struct Slot {
    /// Physical size to reconfigure the surface to.
    resize: Option<(u32, u32)>,
    /// Pre-acquired frame waiting for the GUI thread.
    held: Option<wgpu::SurfaceTexture>,
    /// Whether the GUI thread wants a frame pre-acquired.
    want_frame: bool,
    /// Painted frame waiting to be presented.
    present: Option<wgpu::SurfaceTexture>,
    /// A frame is out with the GUI thread (taken, not yet presented
    /// or discarded). wgpu allows only ONE outstanding acquired
    /// texture per surface, so the pump must not acquire while set.
    taken: bool,
    shutdown: bool,
    /// Set by the pump thread on its way out; `Drop` waits on this
    /// (bounded) to decide between join and detach.
    exited: bool,
}

#[cfg(target_os = "windows")]
struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
    state: AtomicU8,
    /// How long the pump's most recent acquire blocked, in
    /// nanoseconds. Feeds the GUI thread's `PaintPacer` so paint
    /// cadence still tracks the compositor's real consumption rate.
    last_acquire_nanos: AtomicU64,
}

#[cfg(target_os = "windows")]
fn lock(slot: &Mutex<Slot>) -> std::sync::MutexGuard<'_, Slot> {
    // Pump-thread work is wrapped in `catch_unwind`, so poisoning is
    // unexpected; recover rather than deadlock editor teardown.
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- macOS / Linux: inline pump ---

/// Synchronous surface owner for the platforms where swapchain calls
/// stay on the calling thread. `Mutex` only for `Send`/`Clone`; there
/// is no cross-thread contention.
#[cfg(not(target_os = "windows"))]
struct InlineState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    config: wgpu::SurfaceConfiguration,
    last_acquire: std::time::Duration,
}

#[cfg(not(target_os = "windows"))]
fn lock_inline(state: &Mutex<InlineState>) -> std::sync::MutexGuard<'_, InlineState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Cheap cloneable handle for per-frame pump operations: resize,
/// frame acquire, present. The editor's backend keeps one; the
/// [`SurfacePump`] owner keeps the lifecycle.
#[derive(Clone)]
pub struct PumpClient {
    #[cfg(target_os = "windows")]
    shared: Arc<Shared>,
    #[cfg(not(target_os = "windows"))]
    state: Arc<Mutex<InlineState>>,
}

impl PumpClient {
    /// Reconfigure the surface (physical pixels). On Windows this is
    /// queued latest-wins - a resize burst costs one configure once
    /// the pump gets to it, and any pre-acquired frame at the old
    /// size is discarded. Elsewhere it configures inline.
    pub fn resize(&self, phys_w: u32, phys_h: u32) {
        #[cfg(target_os = "windows")]
        {
            let mut slot = lock(&self.shared.slot);
            slot.resize = Some((phys_w, phys_h));
            drop(slot);
            self.shared.cv.notify_all();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut state = lock_inline(&self.state);
            state.config.width = phys_w.max(1);
            state.config.height = phys_h.max(1);
            let InlineState {
                surface,
                device,
                config,
                ..
            } = &mut *state;
            surface.configure(device, config);
        }
    }

    /// Get a frame to paint into. `None` means skip this paint and
    /// keep the dirty state - retry on a later tick. Callers should
    /// verify the texture's size still matches their target and
    /// discard (drop) it if a resize raced in.
    ///
    /// Windows: takes the pre-acquired frame if ready, arming the
    /// pump to acquire the next one either way - never blocks.
    /// Elsewhere: acquires inline (recovering from a stale surface by
    /// reconfiguring), exactly as the editors did before the pump.
    #[must_use]
    pub fn try_take_frame(&self) -> Option<wgpu::SurfaceTexture> {
        #[cfg(target_os = "windows")]
        {
            let mut slot = lock(&self.shared.slot);
            slot.want_frame = true;
            let frame = slot.held.take();
            if frame.is_some() {
                slot.taken = true;
            }
            drop(slot);
            self.shared.cv.notify_all();
            frame
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut state = lock_inline(&self.state);
            let acquire_start = std::time::Instant::now();
            let mut acquired = None;
            // `Outdated` / `Lost` / `Validation` persist until a
            // reconfigure (even same-size clears the flag); `Timeout`
            // / `Occluded` are transient, skip the frame.
            for _ in 0..2 {
                match state.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(frame)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                        acquired = Some(frame);
                        break;
                    }
                    wgpu::CurrentSurfaceTexture::Outdated
                    | wgpu::CurrentSurfaceTexture::Lost
                    | wgpu::CurrentSurfaceTexture::Validation => {
                        let InlineState {
                            surface,
                            device,
                            config,
                            ..
                        } = &mut *state;
                        surface.configure(device, config);
                    }
                    wgpu::CurrentSurfaceTexture::Timeout
                    | wgpu::CurrentSurfaceTexture::Occluded => break,
                }
            }
            state.last_acquire = acquire_start.elapsed();
            acquired
        }
    }

    /// Present a painted frame (handed back to the pump thread on
    /// Windows, presented inline elsewhere).
    pub fn present(&self, frame: wgpu::SurfaceTexture) {
        #[cfg(target_os = "windows")]
        {
            let mut slot = lock(&self.shared.slot);
            // At most one frame is ever out with the GUI thread, so a
            // pending unpresented frame here is impossible; replace-
            // drop is just belt-and-braces.
            slot.present = Some(frame);
            slot.taken = false;
            drop(slot);
            self.shared.cv.notify_all();
        }
        #[cfg(not(target_os = "windows"))]
        {
            frame.present();
        }
    }

    /// Release a taken frame that won't be painted (stale size after
    /// a raced resize, or an aborted paint), so the pump may acquire
    /// again - wgpu allows only one outstanding acquired texture per
    /// surface. On Windows the frame is presented unrendered rather
    /// than dropped: DX12 replenishes the frame-latency waitable only
    /// on present, and a dropped acquire burns a slot until every
    /// acquire blocks wgpu's full 1 s timeout. The recycled old-frame
    /// content matches what the compositor is already showing
    /// mid-churn.
    pub fn discard(&self, frame: wgpu::SurfaceTexture) {
        #[cfg(target_os = "windows")]
        self.present(frame);
        #[cfg(not(target_os = "windows"))]
        drop(frame);
    }

    /// Whether GPU init has completed and frames can be served. Always
    /// true on the inline platforms (init ran synchronously in
    /// `spawn`).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.shared.state.load(Ordering::Acquire) == STATE_READY
        }
        #[cfg(not(target_os = "windows"))]
        {
            true
        }
    }

    /// Whether init failed or the pump thread died.
    #[must_use]
    pub fn failed(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.shared.state.load(Ordering::Acquire) == STATE_FAILED
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    /// The most recent swapchain-acquire wait, for compositor pacing.
    #[must_use]
    pub fn last_acquire_wait(&self) -> std::time::Duration {
        #[cfg(target_os = "windows")]
        {
            std::time::Duration::from_nanos(self.shared.last_acquire_nanos.load(Ordering::Relaxed))
        }
        #[cfg(not(target_os = "windows"))]
        {
            lock_inline(&self.state).last_acquire
        }
    }
}

/// Owning handle for the pump. On Windows, dropping it shuts the
/// thread down (bounded join, then detach - a thread wedged in a
/// driver call is left behind rather than hanging the GUI thread).
pub struct SurfacePump<T: Send + 'static> {
    client: PumpClient,
    init: InitDelivery<T>,
    #[cfg(target_os = "windows")]
    join: Option<std::thread::JoinHandle<()>>,
}

enum InitDelivery<T> {
    /// Inline init: the product is available immediately.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    Now(Option<T>),
    /// Threaded init: the product arrives when the pump thread
    /// finishes.
    #[cfg(target_os = "windows")]
    Chan(std::sync::mpsc::Receiver<T>),
}

impl<T: Send + 'static> SurfacePump<T> {
    /// Build the pump for a baseview window. On Windows this spawns
    /// the pump thread and returns immediately (GPU init runs there;
    /// poll [`Self::take_init`]); elsewhere init runs synchronously
    /// and `take_init` succeeds on the first call.
    ///
    /// `device_lost` is raised if the pump thread panics so the
    /// editor's device-loss recovery can rebuild with a fresh pump.
    ///
    /// # Safety
    /// The window must remain valid while the pump lives - the editor
    /// guarantees this by dropping the pump before closing its child
    /// window (a destroyed HWND fails surface creation with a driver
    /// error, not UB).
    #[cfg(not(target_os = "ios"))]
    pub unsafe fn spawn(
        window: &baseview::Window,
        device_lost: &Arc<AtomicBool>,
        init: PumpInitFn<T>,
    ) -> Option<Self> {
        #[cfg(target_os = "windows")]
        {
            use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
            let RawWindowHandle::Win32(handle) = window.raw_window_handle() else {
                return None;
            };
            let hwnd = handle.hwnd as isize;
            if hwnd == 0 {
                return None;
            }
            Self::spawn_threaded(hwnd, device_lost.clone(), init)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = device_lost;
            let instance = wgpu::Instance::new(super::platform::editor_instance_descriptor());
            let surface = unsafe { super::platform::create_wgpu_surface(&instance, window) }?;
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                }))
                .ok()?;
            let (product, device, config) = init(&instance, &adapter, &surface)?;
            surface.configure(&device, &config);
            Some(Self {
                client: PumpClient {
                    state: Arc::new(Mutex::new(InlineState {
                        surface,
                        device,
                        config,
                        last_acquire: std::time::Duration::ZERO,
                    })),
                },
                init: InitDelivery::Now(Some(product)),
            })
        }
    }

    #[cfg(target_os = "windows")]
    fn spawn_threaded(
        hwnd: isize,
        device_lost: Arc<AtomicBool>,
        init: PumpInitFn<T>,
    ) -> Option<Self> {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            cv: Condvar::new(),
            state: AtomicU8::new(STATE_INIT),
            last_acquire_nanos: AtomicU64::new(0),
        });
        let (init_tx, init_rx) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("truce-surface-pump".into())
            .spawn(move || run(&thread_shared, hwnd, &device_lost, init, &init_tx));
        match spawned {
            Ok(join) => Some(Self {
                client: PumpClient { shared },
                init: InitDelivery::Chan(init_rx),
                join: Some(join),
            }),
            Err(e) => {
                log::error!("surface pump: failed to spawn: {e}");
                None
            }
        }
    }

    /// Poll for the init closure's product (non-blocking). Returns it
    /// exactly once.
    pub fn take_init(&mut self) -> Option<T> {
        match &mut self.init {
            InitDelivery::Now(product) => product.take(),
            #[cfg(target_os = "windows")]
            InitDelivery::Chan(rx) => rx.try_recv().ok(),
        }
    }

    #[must_use]
    pub fn client(&self) -> PumpClient {
        self.client.clone()
    }
}

#[cfg(target_os = "windows")]
impl<T: Send + 'static> Drop for SurfacePump<T> {
    fn drop(&mut self) {
        let mut slot = lock(&self.client.shared.slot);
        slot.shutdown = true;
        self.client.shared.cv.notify_all();
        // Bounded wait; a thread wedged inside the driver can't notice
        // the flag, so detach instead of hanging the GUI thread.
        let (slot, timeout) = self
            .client
            .shared
            .cv
            .wait_timeout_while(slot, std::time::Duration::from_secs(1), |s| !s.exited)
            .unwrap_or_else(PoisonError::into_inner);
        drop(slot);
        if timeout.timed_out() {
            log::warn!("surface pump did not exit within 1s (driver stall?); detaching");
            drop(self.join.take());
        } else if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Create a wgpu surface for a raw Win32 HWND. `Send`-able input, so
/// the pump thread can build its own surface.
///
/// # Safety
/// `hwnd` must be a valid window handle that outlives the surface.
#[cfg(target_os = "windows")]
unsafe fn surface_from_hwnd(
    instance: &wgpu::Instance,
    hwnd: isize,
) -> Option<wgpu::Surface<'static>> {
    let mut win32 = wgpu::rwh::Win32WindowHandle::new(std::num::NonZeroIsize::new(hwnd)?);
    win32.hinstance = super::platform::current_module_hinstance();
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Windows(
            wgpu::rwh::WindowsDisplayHandle::new(),
        )),
        raw_window_handle: wgpu::rwh::RawWindowHandle::Win32(win32),
    };
    unsafe { instance.create_surface_unsafe(target) }.ok()
}

/// Pump thread body: init, then serve resize / acquire / present
/// until shutdown.
#[cfg(target_os = "windows")]
fn run<T: Send>(
    shared: &Shared,
    hwnd: isize,
    device_lost: &Arc<AtomicBool>,
    init: PumpInitFn<T>,
    init_tx: &std::sync::mpsc::Sender<T>,
) {
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let instance = wgpu::Instance::new(super::platform::editor_instance_descriptor());
        // SAFETY: the hwnd outlives the pump - see `SurfacePump::spawn`.
        let surface = unsafe { surface_from_hwnd(&instance, hwnd) }?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok()?;
        let (product, device, config) = init(&instance, &adapter, &surface)?;
        surface.configure(&device, &config);
        Some((product, device, config, surface))
    }))
    .ok()
    .flatten();
    let Some((product, device, mut config, surface)) = built else {
        shared.state.store(STATE_FAILED, Ordering::Release);
        log::error!("surface pump: gpu init failed; editor stays blank");
        mark_exited(shared);
        return;
    };
    // Deliver before flipping state so a GUI that sees READY always
    // finds the product waiting.
    let _ = init_tx.send(product);
    shared.state.store(STATE_READY, Ordering::Release);

    'work: loop {
        let (resize, present, need_acquire) = {
            let mut slot = lock(&shared.slot);
            loop {
                if slot.shutdown {
                    break 'work;
                }
                let need_acquire = slot.want_frame && slot.held.is_none() && !slot.taken;
                // A resize can't be applied while a frame is out with
                // the GUI thread: `surface.configure` panics if any
                // acquired texture is still alive. The GUI's present /
                // discard clears `taken` and notifies.
                let can_resize = slot.resize.is_some() && !slot.taken;
                if can_resize || slot.present.is_some() || need_acquire {
                    break;
                }
                slot = shared.cv.wait(slot).unwrap_or_else(PoisonError::into_inner);
            }
            let need_acquire = slot.want_frame && slot.held.is_none() && !slot.taken;
            let resize = if slot.taken { None } else { slot.resize.take() };
            (resize, slot.present.take(), need_acquire)
        };
        // Everything below can block inside the driver - that is the
        // point of this thread. A panic flags device loss so the
        // editor rebuilds with a fresh pump.
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(frame) = present {
                frame.present();
            }
            if let Some((w, h)) = resize {
                // A frame acquired under the old configuration can't
                // be painted meaningfully - but it must be PRESENTED,
                // not dropped: DX12 replenishes the swapchain's
                // frame-latency waitable only on present, so a dropped
                // acquire burns a latency slot and once starved every
                // subsequent acquire blocks wgpu's full 1 s timeout
                // (measured; it was the multi-second post-resize
                // stall). Its recycled old-frame content matches the
                // stretched frame the compositor is showing anyway.
                if let Some(stale) = lock(&shared.slot).held.take() {
                    stale.present();
                }
                config.width = w.max(1);
                config.height = h.max(1);
                surface.configure(&device, &config);
            }
            // A drag queues resizes faster than configure + acquire
            // can run; if another one is already waiting, coalesce it
            // first - a frame acquired now would only be discarded.
            if need_acquire && lock(&shared.slot).resize.is_none() {
                let acquire_start = std::time::Instant::now();
                let mut acquired = None;
                // Same stale-surface recovery as the inline path.
                for _ in 0..2 {
                    match surface.get_current_texture() {
                        wgpu::CurrentSurfaceTexture::Success(frame)
                        | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                            acquired = Some(frame);
                            break;
                        }
                        wgpu::CurrentSurfaceTexture::Outdated
                        | wgpu::CurrentSurfaceTexture::Lost
                        | wgpu::CurrentSurfaceTexture::Validation => {
                            surface.configure(&device, &config);
                        }
                        wgpu::CurrentSurfaceTexture::Timeout
                        | wgpu::CurrentSurfaceTexture::Occluded => break,
                    }
                }
                let nanos = u64::try_from(acquire_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                shared.last_acquire_nanos.store(nanos, Ordering::Relaxed);
                if let Some(frame) = acquired {
                    let mut slot = lock(&shared.slot);
                    // A resize that raced in invalidates this frame;
                    // present it (see the resize branch - dropping
                    // burns a frame-latency slot) and let the next
                    // loop pass reconfigure + reacquire.
                    if slot.resize.is_none() {
                        slot.held = Some(frame);
                    } else {
                        drop(slot);
                        frame.present();
                    }
                }
            }
        }));
        if ok.is_err() {
            device_lost.store(true, Ordering::Release);
            shared.state.store(STATE_FAILED, Ordering::Release);
            log::error!("surface pump panicked; flagging device loss for rebuild");
            break;
        }
    }
    mark_exited(shared);
}

#[cfg(target_os = "windows")]
fn mark_exited(shared: &Shared) {
    let mut slot = lock(&shared.slot);
    // Frames can't outlive the surface; drop any still queued. The
    // drop itself can panic (wgpu discards the texture against a
    // surface whose configure already failed); swallow it so teardown
    // always completes.
    let held = slot.held.take();
    let present = slot.present.take();
    slot.exited = true;
    drop(slot);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        drop(held);
        drop(present);
    }));
    shared.cv.notify_all();
}
