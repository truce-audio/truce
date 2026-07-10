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

pub use plugin_cell::{PluginCell, PluginGuard};

/// The ownership cell every format wrapper puts around its plugin
/// instance. The audio thread owns the plugin while the host is
/// processing (`process`, the queued state apply); the host thread owns
/// it while processing is stopped (`init`, `reset`, an inactive state
/// load). The host contract makes those two mutually exclusive in time -
/// a spec-compliant host never overlaps `process` with a lifecycle
/// callback - so [`PluginCell`] holds no OS lock and the audio thread
/// never waits. Ownership handoff carries a release-acquire edge (each
/// owner observes the previous owner's writes), not mutual exclusion.
///
/// A host state save no longer touches the plugin at all: it reads the
/// lock-free [`SnapshotSlot`](crate::snapshot::SnapshotSlot) the audio
/// thread publishes each block (see [`save_extra`]). Meters ride the
/// lock-free `MeterStore`, and params are atomic. So nothing on a
/// non-audio thread contends with `process` on the hot path.
///
/// The soundness rests on the host's process/lifecycle exclusion
/// contract; a debug-build overlap detector trips if a host ever
/// violates it. The `Arc` is what makes GUI closures sound: they clone
/// the handle instead of stashing a raw pointer into the instance
/// struct.
pub type SharedPlugin<P> = Arc<PluginCell<P>>;

/// Wrap a freshly created plugin in the wrapper-standard ownership
/// cell. See [`SharedPlugin`].
pub fn shared_plugin<P>(plugin: P) -> SharedPlugin<P> {
    Arc::new(PluginCell::new(plugin))
}

/// Take ownership of the plugin for the current callback. Never blocks:
/// the audio thread owns the plugin while active, the host thread while
/// inactive, and the host contract keeps the two from overlapping, so
/// there is nothing to wait on. The returned guard's `&mut` is exclusive
/// by that contract; the `Acquire` inside observes the previous owner's
/// writes.
pub fn enter_plugin<P>(plugin: &PluginCell<P>) -> PluginGuard<'_, P> {
    plugin.enter()
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

/// Lock-free plugin ownership cell, uniform across platforms. Holds no
/// OS mutex: the audio thread owns the plugin while processing, the host
/// thread while stopped, and a spec-compliant host never overlaps the
/// two. Ownership handoff is a release-acquire edge, not a lock, so
/// `enter` never blocks and there is no poison, no priority inversion,
/// and no per-platform variant.
mod plugin_cell {
    use std::cell::UnsafeCell;
    use std::marker::PhantomData;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct PluginCell<T> {
        data: UnsafeCell<T>,
        /// Release-acquire handoff counter. Each owner `Acquire`s on
        /// entry (observing the previous owner's writes) and `Release`s
        /// on exit (publishing its own), carrying the happens-before edge
        /// between the audio thread and the host thread. Mutual exclusion
        /// comes from the host contract - `process` never overlaps a
        /// lifecycle callback - not from this counter.
        handoff: AtomicU64,
        /// Debug-only overlap detector: trips if two owners ever hold the
        /// cell at once (a host contract violation). Compiled out in
        /// release, where the contract is trusted.
        #[cfg(debug_assertions)]
        held: std::sync::atomic::AtomicBool,
    }

    // SAFETY: `T` is reached only through a guard, and the host contract
    // hands it to one owner at a time - the same guarantee `Mutex<T>`
    // leans on, so `Send`/`Sync` need only `T: Send`.
    unsafe impl<T: Send> Send for PluginCell<T> {}
    unsafe impl<T: Send> Sync for PluginCell<T> {}

    impl<T> PluginCell<T> {
        pub fn new(value: T) -> Self {
            Self {
                data: UnsafeCell::new(value),
                handoff: AtomicU64::new(0),
                #[cfg(debug_assertions)]
                held: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// Take ownership. Never blocks: the previous owner has already
        /// released, by the host contract. The `Acquire` observes its
        /// writes.
        #[allow(
            clippy::missing_panics_doc,
            reason = "the only panic is the debug-only overlap detector, compiled out in release"
        )]
        pub fn enter(&self) -> PluginGuard<'_, T> {
            self.handoff.load(Ordering::Acquire);
            #[cfg(debug_assertions)]
            assert!(
                !self.held.swap(true, Ordering::Relaxed),
                "plugin ownership cell entered while already held: the host \
                 overlapped process() with a lifecycle callback"
            );
            PluginGuard {
                cell: self,
                _not_send: PhantomData,
            }
        }
    }

    /// Guard handing out the exclusive `&mut T`; releases the handoff on
    /// drop so the next owner's `Acquire` sees this owner's writes.
    pub struct PluginGuard<'a, T> {
        cell: &'a PluginCell<T>,
        /// The acquiring thread must also release, for the handoff edge
        /// to mean anything - so the guard can't cross threads.
        _not_send: PhantomData<*const ()>,
    }

    impl<T> Deref for PluginGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: this thread solely owns the cell for the guard's
            // lifetime (host exclusion contract), so no other reference
            // to `data` exists.
            unsafe { &*self.cell.data.get() }
        }
    }

    impl<T> DerefMut for PluginGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: as in `deref` - sole owner, so this `&mut` is unique.
            unsafe { &mut *self.cell.data.get() }
        }
    }

    impl<T> Drop for PluginGuard<'_, T> {
        fn drop(&mut self) {
            #[cfg(debug_assertions)]
            self.cell.held.store(false, Ordering::Relaxed);
            // Release: publish this owner's writes to the next `Acquire`.
            self.cell.handoff.fetch_add(1, Ordering::Release);
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
mod plugin_cell_tests {
    use std::sync::Arc;

    use super::{enter_plugin, shared_plugin};

    #[test]
    fn lock_round_trips_data() {
        let plugin = shared_plugin(41);
        *enter_plugin(&plugin) += 1;
        assert_eq!(*enter_plugin(&plugin), 42);
    }

    #[test]
    fn repeated_ownership_publishes_writes() {
        // Models the audio thread owning the cell block after block:
        // each release-acquire cycle observes the previous cycle's write.
        let plugin = shared_plugin(0u64);
        for _ in 0..1000 {
            *enter_plugin(&plugin) += 1;
        }
        assert_eq!(*enter_plugin(&plugin), 1000);
    }

    #[test]
    fn handoff_carries_writes_across_a_thread() {
        // A non-overlapping handoff (the host contract): the worker owns
        // the cell, writes, and releases; only after it joins does the
        // main thread acquire. The cell's `Acquire` makes the worker's
        // write visible - no overlap, so the detector never trips.
        let plugin = shared_plugin(0u32);
        let worker = {
            let plugin = Arc::clone(&plugin);
            std::thread::spawn(move || {
                *enter_plugin(&plugin) = 99;
            })
        };
        worker.join().unwrap();
        assert_eq!(*enter_plugin(&plugin), 99);
    }

    #[test]
    fn panicking_owner_does_not_wedge_the_cell() {
        // A panic in an owner unwinds through the guard's `Drop`, which
        // releases the handoff (and clears the debug overlap flag), so
        // the cell stays usable - one bad block can't wedge it.
        let plugin = shared_plugin(7);
        let for_panic = Arc::clone(&plugin);
        let _ = std::thread::spawn(move || {
            let _guard = enter_plugin(&for_panic);
            panic!("wedge attempt");
        })
        .join();
        assert_eq!(*enter_plugin(&plugin), 7);
    }
}
