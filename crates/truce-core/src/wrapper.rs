//! Helpers shared across format wrappers (CLAP, VST3, VST2, AU, AAX, LV2).
//!
//! Each wrapper still owns its format-specific descriptor types and
//! callback tables; those don't unify cleanly. What unifies is the
//! "boring" boundary glue: building `CStrings` from `ParamInfo`
//! fields, picking the default bus layout, and resolving install-time
//! name overrides.
//!
//! Each helper is a single small function so the wrappers stay
//! greppable - the per-format vtable construction code reads as
//! "for each param, get cstrings, build descriptor" without inlined
//! `CString::new(...).unwrap_or_default()` boilerplate.
//!
//! Adding a new format wrapper? Reach for these first; only fall back
//! to direct `CString::new` etc. when the format genuinely needs
//! something none of the other formats does.

use std::any::type_name;
use std::ffi::CString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use truce_params::ParamInfo;

use crate::bus::BusLayout;
use crate::export::PluginExport;

pub use plugin_mutex::{PluginGuard, PluginMutex};

/// The mediation lock every format wrapper puts around its plugin
/// instance. The audio thread holds the lock for the duration of a
/// block (`process`, `reset`, the queued state apply); host-thread
/// state callbacks and the editor's `get_state` closure block for
/// the read - safe in that direction, and bounded by the block the
/// audio thread is finishing. Meters ride the lock-free `MeterStore`
/// instead, so per-frame GUI paints never touch this lock.
///
/// The lock is only as strong as the scheduler behind it, so
/// [`PluginMutex`] picks per platform:
/// - **macOS**: `std::sync::Mutex` sits on `os_unfair_lock`, which
///   donates the waiter's priority to the owner - a GUI thread
///   preempted mid-`save_state` gets boosted to the waiting audio
///   thread's priority.
/// - **Linux**: a `PTHREAD_PRIO_INHERIT` pthread mutex; std's
///   futex-based lock has no priority inheritance there.
/// - **Windows**: `std::sync::Mutex` (SRWLOCK). User space has no
///   priority-inheriting primitive, so the defense is the short
///   hold - non-audio holders only span `save_state` / `editor()`.
///
/// (`parking_lot` inherits priority nowhere, which is why it isn't
/// used. Uncontended cost is a CAS on every platform.)
/// A panic while holding the guard unlocks on unwind; std's poison
/// is forgiven (see [`lock_plugin`]).
///
/// A `Mutex` rather than an `RwLock`: the only non-audio accessors
/// are a host state save and an editor preset capture - both rare -
/// so reader parallelism buys nothing, and `Mutex<P>: Sync` needs
/// only `P: Send` (an `RwLock` would force `Sync` onto every plugin
/// type).
///
/// The `Arc` is what makes GUI closures sound: they clone the handle
/// instead of stashing a raw pointer into the instance struct (whose
/// `&mut` the audio thread holds during callbacks).
pub type SharedPlugin<P> = Arc<PluginMutex<P>>;

/// Wrap a freshly created plugin in the wrapper-standard mediation
/// lock. See [`SharedPlugin`].
pub fn shared_plugin<P>(plugin: P) -> SharedPlugin<P> {
    let shared = Arc::new(PluginMutex::new(plugin));
    // Warm the lock off the audio thread: the std-backed variant (macOS
    // boxes a `pthread_mutex_t`) lazily allocates the OS mutex on first
    // lock, which would otherwise land on the first audio callback that
    // takes the mediation lock. Locking here forces that one-time init at
    // creation. No-op on the Linux pthread variant (its mutex is built in
    // `new`) and on Windows (SRWLOCK is inline).
    drop(shared.lock());
    shared
}

/// Lock the mediation lock, forgiving poison. A poisoned lock means a
/// panic already escaped somewhere and was reported by the wrapper's
/// panic guard; refusing every later block would turn one bad block
/// into permanent silence, and the plugin's state is no more suspect
/// than after any other caught panic. (The Linux pthread lock has no
/// poison to forgive; unwind simply unlocks.)
pub fn lock_plugin<P>(plugin: &PluginMutex<P>) -> PluginGuard<'_, P> {
    plugin.lock()
}

/// [`lock_plugin`]'s non-blocking twin: `None` only when the lock is
/// genuinely held (poison is forgiven, same rationale).
pub fn try_lock_plugin<P>(plugin: &PluginMutex<P>) -> Option<PluginGuard<'_, P>> {
    plugin.try_lock()
}

/// Read the plugin's custom-state blob for a host state save.
///
/// Reads the lock-free [`SnapshotSlot`](crate::snapshot::SnapshotSlot)
/// the audio thread publishes each block, and never takes the plugin
/// lock: a host save must not stall the audio thread, which holds that
/// lock for the whole block. A plugin with custom state publishes it
/// through `snapshot_into`; one that publishes nothing has no custom
/// state to save, so an empty blob is correct. The wrapper republishes
/// the snapshot after any state change applied outside `process` (an
/// inactive load), so an inactive save still returns live state. Params
/// are serialized separately (also lock-free).
#[must_use]
pub fn save_extra(snapshot: &crate::snapshot::SnapshotSlot) -> Vec<u8> {
    snapshot.read().unwrap_or_default()
}

/// std-backed [`PluginMutex`]: macOS (`os_unfair_lock` donates the
/// waiter's priority) and Windows (SRWLOCK; no user-space priority
/// inheritance exists). Miri also lands here - it has no shim for
/// `pthread_mutexattr_setprotocol`, and the std lock gives it full
/// visibility.
#[cfg(any(not(target_os = "linux"), miri))]
mod plugin_mutex {
    use std::ops::{Deref, DerefMut};
    use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

    /// See [`super::SharedPlugin`] for the per-platform lock choice.
    pub struct PluginMutex<T>(Mutex<T>);

    /// Guard handing out the exclusive `&mut T`; unlocks on drop.
    pub struct PluginGuard<'a, T>(MutexGuard<'a, T>);

    impl<T> PluginMutex<T> {
        pub fn new(value: T) -> Self {
            Self(Mutex::new(value))
        }

        /// Block until the lock is held. Poison is forgiven (see
        /// [`super::lock_plugin`]).
        pub fn lock(&self) -> PluginGuard<'_, T> {
            PluginGuard(self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }

        /// `None` only when the lock is genuinely held.
        pub fn try_lock(&self) -> Option<PluginGuard<'_, T>> {
            match self.0.try_lock() {
                Ok(guard) => Some(PluginGuard(guard)),
                Err(TryLockError::Poisoned(poisoned)) => Some(PluginGuard(poisoned.into_inner())),
                Err(TryLockError::WouldBlock) => None,
            }
        }
    }

    impl<T> Deref for PluginGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }

    impl<T> DerefMut for PluginGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.0
        }
    }
}

/// Linux [`PluginMutex`]: a `PTHREAD_PRIO_INHERIT` pthread mutex.
/// std's futex-based lock has no priority inheritance, so a
/// low-priority GUI thread preempted mid-`save_state` would stall
/// the audio thread at the scheduler's mercy; with PI the holder
/// inherits the waiting audio thread's priority for the remainder
/// of the hold.
#[cfg(all(target_os = "linux", not(miri)))]
mod plugin_mutex {
    use std::cell::UnsafeCell;
    use std::marker::PhantomData;
    use std::ops::{Deref, DerefMut};

    /// See [`super::SharedPlugin`] for the per-platform lock choice.
    pub struct PluginMutex<T> {
        /// Boxed: a pthread mutex must not move once initialized,
        /// and `Arc::new(PluginMutex::new(..))` moves the struct.
        raw: Box<UnsafeCell<libc::pthread_mutex_t>>,
        data: UnsafeCell<T>,
    }

    // SAFETY: the pthread mutex serializes all access to `data`, so
    // sharing the container across threads hands `T` to one thread
    // at a time - the same bound (`T: Send`) std's `Mutex` requires.
    unsafe impl<T: Send> Send for PluginMutex<T> {}
    // SAFETY: as above - `&PluginMutex` only reaches `T` through the
    // lock, so `Sync` needs only `T: Send`.
    unsafe impl<T: Send> Sync for PluginMutex<T> {}

    impl<T> PluginMutex<T> {
        pub fn new(value: T) -> Self {
            let raw = Box::new(UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER));
            // SAFETY: `raw` is freshly allocated and unshared; the
            // attr is initialized before use and destroyed after.
            unsafe {
                let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                if libc::pthread_mutexattr_init(&raw mut attr) == 0 {
                    // Best effort: a libc refusing PI still leaves a
                    // valid default-protocol attr, and init below
                    // yields an ordinary mutex.
                    let _ = libc::pthread_mutexattr_setprotocol(
                        &raw mut attr,
                        libc::PTHREAD_PRIO_INHERIT,
                    );
                    let _ = libc::pthread_mutex_init(raw.get(), &raw const attr);
                    let _ = libc::pthread_mutexattr_destroy(&raw mut attr);
                }
            }
            Self {
                raw,
                data: UnsafeCell::new(value),
            }
        }

        /// Block until the lock is held. A panicking previous holder
        /// unlocked on unwind (guard drop); there is no poison state.
        pub fn lock(&self) -> PluginGuard<'_, T> {
            // SAFETY: the mutex was initialized in `new` and outlives
            // the returned guard's borrow.
            unsafe {
                libc::pthread_mutex_lock(self.raw.get());
            }
            PluginGuard {
                lock: self,
                _not_send: PhantomData,
            }
        }

        /// `None` when the lock is held elsewhere.
        pub fn try_lock(&self) -> Option<PluginGuard<'_, T>> {
            // SAFETY: as in `lock`.
            (unsafe { libc::pthread_mutex_trylock(self.raw.get()) } == 0).then_some(PluginGuard {
                lock: self,
                _not_send: PhantomData,
            })
        }
    }

    impl<T> Drop for PluginMutex<T> {
        fn drop(&mut self) {
            // SAFETY: `&mut self` proves no guard is alive, so the
            // mutex is unlocked and safe to destroy.
            unsafe {
                libc::pthread_mutex_destroy(self.raw.get());
            }
        }
    }

    /// Guard handing out the exclusive `&mut T`; unlocks on drop.
    pub struct PluginGuard<'a, T> {
        lock: &'a PluginMutex<T>,
        /// PI mutexes must be unlocked by the locking thread; the
        /// raw-pointer marker strips `Send` so the guard can't cross.
        _not_send: PhantomData<*const ()>,
    }

    impl<T> Deref for PluginGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: this guard holds the lock.
            unsafe { &*self.lock.data.get() }
        }
    }

    impl<T> DerefMut for PluginGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: this guard holds the lock exclusively.
            unsafe { &mut *self.lock.data.get() }
        }
    }

    impl<T> Drop for PluginGuard<'_, T> {
        fn drop(&mut self) {
            // SAFETY: this guard holds the lock; unlock happens on
            // the locking thread (the guard is `!Send`).
            unsafe {
                libc::pthread_mutex_unlock(self.lock.raw.get());
            }
        }
    }
}

/// `CStrings` derived from a single `ParamInfo`. All four conversions
/// follow the same pattern (`unwrap_or_default()` so a `\0` in metadata
/// degrades to an empty C string instead of panicking the host); pulling
/// them into one struct keeps the per-format vtable loops uniform.
pub struct ParamCStrings {
    pub name: CString,
    pub short_name: CString,
    pub unit: CString,
    pub group: CString,
}

impl ParamCStrings {
    /// Build all four `CStrings` for one parameter.
    #[must_use]
    pub fn from_info(info: &ParamInfo) -> Self {
        Self {
            name: CString::new(info.name).unwrap_or_default(),
            short_name: CString::new(info.short_name).unwrap_or_default(),
            unit: CString::new(info.unit.as_str()).unwrap_or_default(),
            group: CString::new(info.group).unwrap_or_default(),
        }
    }
}

/// `(input_channels, output_channels)` for the plugin's default bus
/// layout, or `None` when the plugin declares no layouts.
/// Used by every format's vtable / descriptor to advertise channel
/// counts at registration time.
///
/// **Note for `aumi` (MIDI processor) plugins:** the convention is
/// `bus_layouts: [BusLayout::new()]`, which has zero input *and* zero
/// output channels. This helper returns `Some((0, 0))` for that case,
/// which is correct for AU (the AU shim's `channelCapabilities`
/// returns `[0, 0]` and the host treats the plugin as MIDI-only) but
/// **wrong for AAX**, which requires every plugin to advertise at
/// least stereo audio I/O. AAX maps `(0, 0)` to `(2, 2)` (synthesizing
/// a stereo passthrough) after this helper returns. Don't push that
/// remap into this helper; only AAX needs it.
///
/// `None` indicates a plugin-author bug: zero-bus plugins must return
/// `vec![BusLayout::new()]` explicitly. Callers should log a
/// diagnostic and skip registration (see how each `register_*` entry
/// point handles this) rather than substitute a silent default that
/// would misreport channel counts to the host.
#[must_use]
pub fn default_io_channels<P: PluginExport>() -> Option<(u32, u32)> {
    P::bus_layouts()
        .first()
        .map(|l| (l.total_input_channels(), l.total_output_channels()))
}

/// `(max_input_channels, max_output_channels)` across every declared bus
/// layout, or `None` when the plugin declares no layouts. Wrappers that
/// let the host switch layouts at runtime (AU's per-instance stream
/// format) size their process-time scratch to this so a later, wider
/// layout selection doesn't outgrow buffers allocated for the first one.
#[must_use]
pub fn max_io_channels<P: PluginExport>() -> Option<(u32, u32)> {
    P::bus_layouts().iter().fold(None, |acc, l| {
        let (in_, out) = (l.total_input_channels(), l.total_output_channels());
        Some(acc.map_or((in_, out), |(ai, ao): (u32, u32)| {
            (ai.max(in_), ao.max(out))
        }))
    })
}

/// Pick the plugin's first bus layout, or `None` when the plugin
/// declares no layouts.
/// Used by wrappers (AAX, VST2) that need to read the layout *before*
/// host-side bus-config negotiation, where a missing layout would
/// otherwise produce silently-misreported channel counts.
///
/// For `aumi` plugins the returned layout is typically `BusLayout::new()`
/// (zero in / zero out). AAX synthesizes `(2, 2)` from that case in
/// `register_aax`; see [`default_io_channels`] for the rationale.
///
/// `None` is the same plugin-author-bug indicator as
/// [`default_io_channels`]: log a diagnostic and skip registration.
#[must_use]
pub fn first_bus_layout<P: PluginExport>() -> Option<BusLayout> {
    P::bus_layouts().into_iter().next()
}

/// Find the `bus_layouts()` index whose total input/output channel counts
/// match `(inputs, outputs)`. Wrappers that negotiate a layout from a
/// host-proposed arrangement (VST3 `setBusArrangements`, AU channel-config
/// selection, the standalone device match, VST2's fixed I/O at load) use
/// this to map a request onto a supported layout. `None` when nothing
/// matches; the caller then rejects the arrangement or falls back to the
/// first layout.
#[must_use]
pub fn find_bus_layout<P: PluginExport>(inputs: u32, outputs: u32) -> Option<usize> {
    P::bus_layouts()
        .iter()
        .position(|l| l.total_input_channels() == inputs && l.total_output_channels() == outputs)
}

/// Standard diagnostic emitted by `register_*` when [`first_bus_layout`]
/// or [`default_io_channels`] returns `None`. Centralised so every
/// wrapper prints the same actionable message.
pub fn log_missing_bus_layout<P: PluginExport>(format: &str) {
    eprintln!(
        "[truce {format}] {}::bus_layouts() returned an empty list - \
         plugin will not register. Plugins with no audio I/O (e.g. \
         aumi MIDI-effects) should return vec![BusLayout::new()] \
         explicitly.",
        type_name::<P>(),
    );
}

/// Diagnostic for a plugin that declared more MIDI ports than the
/// format can carry. The wrapper clamps to a single port and routes
/// all traffic to port `0`; without this line the truncation would read
/// as "multi-port supported." `declared` is the plugin's per-direction
/// port count; nothing is logged for the single-port (or zero-port)
/// case. `direction` is `"input"` / `"output"`.
pub fn log_midi_ports_clamped(format: &str, direction: &str, declared: u8) {
    if declared > 1 {
        eprintln!(
            "[truce {format}] plugin declares {declared} MIDI {direction} ports, but {format} \
             carries one - routing all {direction} MIDI to port 0.",
        );
    }
}

/// Run a `register_*` body under [`std::panic::catch_unwind`].
///
/// Format wrappers' `register_*` entry points run during plugin
/// registration - some from `extern "C" fn init` static
/// initializers (`.init_array` / `__mod_init_func` / `.CRT$XCU`),
/// others lazily on the first host query (AAX, to keep the Windows
/// loader-lock window empty during Pro Tools' scan). A panic that
/// escapes them crosses an `extern "C"`
/// boundary and aborts the host process - a `panic = "abort"`
/// configuration would do the same. Catching the unwind here turns
/// any panic during registration into a logged diagnostic plus
/// "host sees no plugin," which is the same outcome a plugin author
/// would expect from a missing `bus_layouts` declaration.
///
/// `AssertUnwindSafe` is applied internally - the panic is treated
/// as fatal-for-this-plugin, so leaving an `Arc` ref-count or
/// `OnceLock` half-set is acceptable: the host won't load the
/// plugin and the process will exit shortly after registration
/// finishes anyway.
pub fn run_register<P>(format: &str, body: impl FnOnce()) {
    let result = catch_unwind(AssertUnwindSafe(body));
    if let Err(payload) = result {
        eprintln!(
            "[truce {format}] panic during register for {}: {}",
            type_name::<P>(),
            extract_panic_msg(&payload),
        );
    }
}

/// Run a per-block audio-thread `body` under
/// [`std::panic::catch_unwind`].
///
/// Format wrappers call this around the `cb_process` body so a panic
/// from user `process()` can't unwind across the `extern "C"` FFI
/// boundary into the host (UB on most toolchains; abort on others).
/// Returns `true` on clean exit, `false` if the body panicked - the
/// caller should zero output buffers on `false` so the host doesn't
/// keep playing whatever happened to be in those slots.
///
/// Panic logging is one short `eprintln!` per occurrence; the audio
/// thread should never panic, so the I/O is rare and acceptable.
#[must_use]
pub fn run_audio_block<P>(format: &str, body: impl FnOnce()) -> bool {
    let result = catch_unwind(AssertUnwindSafe(body));
    if let Err(payload) = result {
        eprintln!(
            "[truce {format}] panic in process() for {}: {}",
            type_name::<P>(),
            extract_panic_msg(&payload),
        );
        return false;
    }
    true
}

/// Like [`run_audio_block`] but for callbacks that return a status
/// code. Returns `body`'s value on a clean exit, `fallback` if the
/// body panicked. Used by the CLAP wrapper, whose process callback
/// returns a `clap_process_status` `i32`.
pub fn run_audio_block_with<P, R>(format: &str, fallback: R, body: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(r) => r,
        Err(payload) => {
            eprintln!(
                "[truce {format}] panic in process() for {}: {}",
                type_name::<P>(),
                extract_panic_msg(&payload),
            );
            fallback
        }
    }
}

/// Run a generic `extern "C"` callback body under
/// [`std::panic::catch_unwind`]. Returns `body`'s value on a clean
/// exit, `fallback` if the body panicked.
///
/// Same shape as [`run_audio_block_with`] but parameterized on
/// `action` (e.g. `"save_state"`, `"load_state"`) so the panic log
/// pinpoints which callback boundary fired. Use this for non-process
/// FFI surfaces - state save / load, param formatting, anything the
/// host calls through an `extern "C" fn` where a panic would unwind
/// across an ABI that doesn't promise abort-on-unwind.
///
/// Audio-thread process bodies should keep using
/// [`run_audio_block`] / [`run_audio_block_with`] - the hardcoded
/// `"process()"` label there keeps existing log lines stable.
pub fn run_extern_callback_with<P, R>(
    format: &str,
    action: &str,
    fallback: R,
    body: impl FnOnce() -> R,
) -> R {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(r) => r,
        Err(payload) => {
            eprintln!(
                "[truce {format}] panic in {action} for {}: {}",
                type_name::<P>(),
                extract_panic_msg(&payload),
            );
            fallback
        }
    }
}

fn extract_panic_msg(payload: &Box<dyn std::any::Any + Send>) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

#[cfg(test)]
mod plugin_mutex_tests {
    use std::sync::Arc;

    use super::{lock_plugin, shared_plugin, try_lock_plugin};

    #[test]
    fn lock_round_trips_data() {
        let plugin = shared_plugin(41);
        *lock_plugin(&plugin) += 1;
        assert_eq!(*lock_plugin(&plugin), 42);
    }

    #[test]
    fn try_lock_reports_contention() {
        let plugin = shared_plugin(0u32);
        let held = lock_plugin(&plugin);
        assert!(try_lock_plugin(&plugin).is_none());
        drop(held);
        assert!(try_lock_plugin(&plugin).is_some());
    }

    #[test]
    fn excludes_across_threads() {
        // Unsynchronized increments would lose updates; the final
        // count proves the guard serializes every access.
        let plugin = shared_plugin(0u64);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let plugin = Arc::clone(&plugin);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        *lock_plugin(&plugin) += 1;
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(*lock_plugin(&plugin), 40_000);
    }

    #[test]
    fn panicking_holder_does_not_wedge_the_lock() {
        // One bad block must not turn into permanent silence: a
        // panicking holder unlocks on unwind (std poison forgiven,
        // pthread unlocked by the guard drop).
        let plugin = shared_plugin(7);
        let for_panic = Arc::clone(&plugin);
        let _ = std::thread::spawn(move || {
            let _guard = lock_plugin(&for_panic);
            panic!("wedge attempt");
        })
        .join();
        assert_eq!(*lock_plugin(&plugin), 7);
        assert!(try_lock_plugin(&plugin).is_some());
    }
}
