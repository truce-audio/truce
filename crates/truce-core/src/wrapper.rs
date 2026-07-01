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

use truce_params::ParamInfo;

use crate::bus::BusLayout;
use crate::export::PluginExport;

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
