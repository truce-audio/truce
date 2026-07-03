//! Dedicated render thread (Windows).
//!
//! Every blocking wgpu call - GPU init, swapchain configure, acquire,
//! present - can park the calling thread inside the graphics driver for
//! an unbounded time (a stalled AMD driver was measured blocking kernel
//! calls *forever*, unkillably). On Windows `on_frame` runs on the
//! host's GUI thread, so those calls used to freeze the entire DAW.
//!
//! This module moves the renderer to its own thread. The GUI thread
//! runs egui and ships each frame's paint payload ([`FramePacket`])
//! through a depth-1 latest-wins slot; the render thread owns the
//! [`EguiRenderer`] and does all the blocking work. If the driver
//! stalls, the render thread sleeps in the driver while the slot keeps
//! absorbing newer frames - the host stays responsive and paints resume
//! wherever the driver left off.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use crate::renderer::EguiRenderer;

/// One egui frame's paint payload, produced on the GUI thread
/// (egui run + tessellate) and consumed by the render thread.
pub struct FramePacket {
    pub textures_delta: egui::TexturesDelta,
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub pixels_per_point: f32,
}

/// GPU init still running on the render thread.
const STATE_INIT: u8 = 0;
/// Renderer built; the thread is consuming frames.
const STATE_READY: u8 = 1;
/// Init failed (or the thread died); the editor stays blank until a
/// device-loss recovery spawns a fresh thread.
const STATE_FAILED: u8 = 2;

/// Depth-1 mailbox between the GUI thread and the render thread.
/// `frame` and `resize` are latest-wins: a submit while the render
/// thread is busy replaces the pending value instead of queueing, so a
/// driver stall never builds a backlog (texture deltas from replaced
/// frames are merged forward - see [`RenderThread::submit`]).
#[derive(Default)]
struct Slot {
    frame: Option<FramePacket>,
    resize: Option<(u32, u32)>,
    shutdown: bool,
    /// Set by the render thread on its way out; `Drop` waits on this
    /// (bounded) to decide between join and detach.
    exited: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
    state: AtomicU8,
    /// How long the render thread's most recent swapchain acquire
    /// blocked, in nanoseconds. The GUI thread feeds this to its
    /// `PaintPacer` so egui runs (and frame submissions) slow to the
    /// compositor's real consumption rate instead of tessellating
    /// frames the slot will just drop.
    last_acquire_nanos: AtomicU64,
}

fn lock(slot: &Mutex<Slot>) -> std::sync::MutexGuard<'_, Slot> {
    // The render thread wraps its work in `catch_unwind`, so poisoning
    // is unexpected; recover rather than deadlock the editor teardown.
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// GUI-thread handle to the render thread. Dropping it shuts the
/// thread down: bounded join, then detach (a thread wedged in a driver
/// call is left behind exactly like the old init worker was).
pub struct RenderThread {
    shared: Arc<Shared>,
    join: Option<std::thread::JoinHandle<()>>,
    /// Whether the GUI thread has observed (and reacted to) the
    /// `STATE_READY` transition - see [`Self::take_ready`].
    adopted: bool,
}

impl RenderThread {
    /// Spawn the render thread. It creates the wgpu instance, surface,
    /// and renderer itself, then loops consuming the slot; the GUI
    /// thread never waits on GPU init at all (the editor is blank
    /// until [`Self::is_ready`], usually well before the first frame).
    ///
    /// `hwnd` must stay valid while the editor's child window is open,
    /// which the editor guarantees (a destroyed HWND fails surface
    /// creation with a driver error, not UB).
    pub fn spawn(
        hwnd: isize,
        width: u32,
        height: u32,
        device_lost: Arc<AtomicBool>,
    ) -> Option<Self> {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            cv: Condvar::new(),
            state: AtomicU8::new(STATE_INIT),
            last_acquire_nanos: AtomicU64::new(0),
        });
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("truce-egui-render".into())
            .spawn(move || run(&thread_shared, hwnd, width, height, &device_lost));
        match spawned {
            Ok(join) => Some(Self {
                shared,
                join: Some(join),
                adopted: false,
            }),
            Err(e) => {
                log::error!("egui render thread: failed to spawn: {e}");
                None
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) == STATE_READY
    }

    /// True exactly once, when GPU init has completed since the last
    /// call - the editor uses it to force a first paint of the blank
    /// window.
    pub fn take_ready(&mut self) -> bool {
        if !self.adopted && self.is_ready() {
            self.adopted = true;
            true
        } else {
            false
        }
    }

    /// Queue a surface reconfigure (physical pixels). Latest-wins: a
    /// burst of resizes costs one configure once the render thread
    /// gets to it. Safe to call before init completes - the thread
    /// applies the pending value right after building the renderer.
    pub fn resize(&self, width: u32, height: u32) {
        let mut slot = lock(&self.shared.slot);
        slot.resize = Some((width, height));
        drop(slot);
        self.shared.cv.notify_all();
    }

    /// Submit a frame to paint. If the previous frame was never
    /// consumed, its texture deltas are merged in front of this one -
    /// deltas mutate renderer state (font atlas uploads) and must all
    /// be applied in order even when the paint itself is skipped.
    pub fn submit(&self, mut packet: FramePacket) {
        let mut slot = lock(&self.shared.slot);
        if let Some(mut prev) = slot.frame.take() {
            prev.textures_delta
                .set
                .append(&mut packet.textures_delta.set);
            prev.textures_delta
                .free
                .append(&mut packet.textures_delta.free);
            packet.textures_delta = prev.textures_delta;
        }
        slot.frame = Some(packet);
        drop(slot);
        self.shared.cv.notify_all();
    }

    /// The render thread's most recent swapchain-acquire wait.
    pub fn last_acquire_wait(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.shared.last_acquire_nanos.load(Ordering::Relaxed))
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        let mut slot = lock(&self.shared.slot);
        slot.shutdown = true;
        self.shared.cv.notify_all();
        // Bounded wait for the thread to notice. A thread wedged in a
        // driver call can't notice; detach it rather than hang the
        // host's GUI thread here (same leak the init worker accepted).
        let (slot, timeout) = self
            .shared
            .cv
            .wait_timeout_while(slot, std::time::Duration::from_secs(1), |s| !s.exited)
            .unwrap_or_else(PoisonError::into_inner);
        drop(slot);
        if timeout.timed_out() {
            log::warn!("egui render thread did not exit within 1s (driver stall?); detaching");
            drop(self.join.take());
        } else if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Extract the Win32 HWND from a baseview window, as a `Send`-able
/// integer the render thread can build its surface from.
pub fn hwnd_for(window: &baseview::Window) -> Option<isize> {
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    let RawWindowHandle::Win32(handle) = window.raw_window_handle() else {
        return None;
    };
    let hwnd = handle.hwnd as isize;
    (hwnd != 0).then_some(hwnd)
}

/// Render thread body: init, then consume the slot until shutdown.
fn run(shared: &Shared, hwnd: isize, width: u32, height: u32, device_lost: &Arc<AtomicBool>) {
    let init = std::panic::catch_unwind(|| {
        // SAFETY: `hwnd` outlives the render thread's renderer - see
        // `RenderThread::spawn`.
        unsafe {
            let instance = wgpu::Instance::new(truce_gui::platform::editor_instance_descriptor());
            truce_gui::platform::create_wgpu_surface_from_hwnd(&instance, hwnd).and_then(
                |surface| {
                    EguiRenderer::init_with_surface(
                        &instance,
                        surface,
                        width,
                        height,
                        device_lost.clone(),
                    )
                },
            )
        }
    })
    .ok()
    .flatten();
    let Some(mut renderer) = init else {
        shared.state.store(STATE_FAILED, Ordering::Release);
        log::error!("egui render thread: gpu init failed; editor stays blank");
        mark_exited(shared);
        return;
    };
    shared.state.store(STATE_READY, Ordering::Release);

    'work: loop {
        let (resize, frame) = {
            let mut slot = lock(&shared.slot);
            loop {
                if slot.shutdown {
                    break 'work;
                }
                if slot.resize.is_some() || slot.frame.is_some() {
                    break;
                }
                slot = shared.cv.wait(slot).unwrap_or_else(PoisonError::into_inner);
            }
            (slot.resize.take(), slot.frame.take())
        };
        // Blocking driver work happens here, off the host's GUI
        // thread. A panic (wgpu validation, poisoned device) flags
        // device loss so the editor rebuilds with a fresh thread.
        let painted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some((w, h)) = resize {
                renderer.resize(w, h);
            }
            if let Some(f) = frame {
                renderer.render(&f.textures_delta, &f.primitives, f.pixels_per_point);
                let nanos = u64::try_from(renderer.acquire_wait().as_nanos()).unwrap_or(u64::MAX);
                shared.last_acquire_nanos.store(nanos, Ordering::Relaxed);
            }
        }));
        if painted.is_err() {
            device_lost.store(true, Ordering::Release);
            shared.state.store(STATE_FAILED, Ordering::Release);
            log::error!("egui render thread panicked; flagging device loss for rebuild");
            break;
        }
    }
    mark_exited(shared);
}

fn mark_exited(shared: &Shared) {
    let mut slot = lock(&shared.slot);
    slot.exited = true;
    drop(slot);
    shared.cv.notify_all();
}
