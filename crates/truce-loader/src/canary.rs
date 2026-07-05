//! ABI canary - runtime verification that shell and dylib have
//! compatible type layouts and vtable ordering.

use std::cell::RefCell;
use std::mem::{align_of, size_of};
use std::ptr;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo as Transport};
use truce_core::process::{ProcessContext, ProcessStatus};
// Source canary types from `truce-gui-types` (the lightweight types
// crate) and `truce-plugin` (the trait surface) so the canary - which
// every shell needs - stays available even when `builtin-gui` is off
// and the heavy `truce-gui` renderer crate is out of the dep graph.
use truce_gui_types::interaction::WidgetRegion;
use truce_gui_types::layout::GridLayout;
use truce_gui_types::theme::{Color, Theme};
use truce_params::sample::Sample;
use truce_plugin::PluginLogicCore;

/// Hand-bumped ABI epoch. Sizes and alignments can't see every
/// layout change: `Event::port` landed in former padding, so
/// `event_size` stayed the same while a stale shell would read
/// uninitialized padding as the port. Bump this when any
/// boundary-crossing type changes layout invisibly to the size /
/// align fields below. When `AbiCanary` itself gains or loses a
/// field, bump the *export symbol* version instead
/// (`truce_abi_canary_vN`) - the canary crosses the boundary by
/// value, so two different canary layouts must never call each
/// other.
///
/// Epoch 2: `Event` grew `port: u8` in former padding (multi-port
/// MIDI).
/// Epoch 3: `editor` left the `PluginLogicCore` vtable (it moved to the
/// receiverless `truce_build_editor` export). The slot's removal shifts
/// every later vtable index, so a stale epoch-2 dylib would bind
/// `save_state` / `latency` / `tail` to the wrong slots and lacks the
/// new symbol; this bump rejects it cleanly at the canary instead of
/// relying on the probe to notice the misalignment.
pub const ABI_EPOCH: u32 = 3;

/// ABI fingerprint. Compared between shell and dylib before loading.
///
/// This is the ONE `#[repr(C)]` type in the system - it's the
/// bootstrap verification struct that makes everything else safe.
#[repr(C)]
pub struct AbiCanary {
    /// [`ABI_EPOCH`] the side was built with; see its rules for when
    /// to bump what.
    pub abi_epoch: u32,
    pub trait_object_size: usize,
    pub audio_buffer_size: usize,
    pub process_context_size: usize,
    pub process_status_size: usize,
    pub event_size: usize,
    pub event_body_size: usize,
    pub transport_size: usize,
    pub widget_region_size: usize,
    pub theme_size: usize,
    pub plugin_layout_size: usize,
    pub color_size: usize,
    pub vec_u8_size: usize,
    pub option_usize_size: usize,
    pub audio_buffer_align: usize,
    pub process_status_align: usize,
    pub result_normal_disc: u8,
    pub result_tail_disc: u8,
    pub result_keepalive_disc: u8,
    pub rustc_version_hash: u64,
    /// Bit-width of the plugin's chosen sample type - `32` for `f32`,
    /// `64` for `f64`. Without this field, a shell built against
    /// `prelude` (f32) loading a logic dylib built against `prelude64`
    /// would bind to a vtable whose `process()` slot expects
    /// `AudioBuffer<f64>` - silent UB on the first audio block. The
    /// width difference between the two `AudioBuffer<S>` instantiations
    /// (and `dyn PluginLogic<S>`) is invisible at the dyn-trait
    /// boundary, so a structural canary alone wouldn't catch it.
    pub sample_precision: u8,
}

impl AbiCanary {
    /// Build the canary for a specific sample precision `S`. The
    /// shell calls this with its own `S`; the dylib's
    /// `truce_abi_canary_v2` export does the same with its own (from the
    /// prelude alias). The two are compared at load time.
    #[must_use]
    pub fn current<S: truce_params::sample::Sample>() -> Self {
        // 8× sizeof gives us 32 for f32 / 64 for f64; the cast to u8
        // can't overflow for any plausible sample type.
        #[allow(clippy::cast_possible_truncation)]
        let sample_precision = (size_of::<S>() * 8) as u8;
        Self {
            abi_epoch: ABI_EPOCH,
            trait_object_size: size_of::<*const dyn PluginLogicCore<S>>() * 2,
            audio_buffer_size: size_of::<AudioBuffer<S>>(),
            process_context_size: size_of::<ProcessContext>(),
            process_status_size: size_of::<ProcessStatus>(),
            event_size: size_of::<Event>(),
            event_body_size: size_of::<EventBody>(),
            transport_size: size_of::<Transport>(),
            widget_region_size: size_of::<WidgetRegion>(),
            theme_size: size_of::<Theme>(),
            plugin_layout_size: size_of::<GridLayout>(),
            color_size: size_of::<Color>(),
            vec_u8_size: size_of::<Vec<u8>>(),
            option_usize_size: size_of::<Option<usize>>(),
            audio_buffer_align: align_of::<AudioBuffer<S>>(),
            process_status_align: align_of::<ProcessStatus>(),
            result_normal_disc: discriminant_byte(&ProcessStatus::Normal),
            result_tail_disc: discriminant_byte(&ProcessStatus::Tail(0)),
            result_keepalive_disc: discriminant_byte(&ProcessStatus::KeepAlive),
            rustc_version_hash: rustc_hash(),
            sample_precision,
        }
    }

    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.field_diffs(other).is_empty()
    }

    #[must_use]
    pub fn diff_report(&self, other: &Self) -> String {
        let diffs = self.field_diffs(other);
        if diffs.is_empty() {
            "no differences".into()
        } else {
            format!("ABI mismatches:\n{}", diffs.join("\n"))
        }
    }

    fn field_diffs(&self, other: &Self) -> Vec<String> {
        let mut diffs = Vec::new();
        macro_rules! check {
            ($field:ident) => {
                if self.$field != other.$field {
                    diffs.push(format!(
                        "  {}: shell={}, dylib={}",
                        stringify!($field),
                        self.$field,
                        other.$field
                    ));
                }
            };
        }
        // Single source of truth - adding a field to AbiCanary means
        // adding one line below; `matches` and `diff_report` both
        // reuse this list.
        check!(abi_epoch);
        check!(trait_object_size);
        check!(audio_buffer_size);
        check!(process_context_size);
        check!(process_status_size);
        check!(event_size);
        check!(event_body_size);
        check!(transport_size);
        check!(widget_region_size);
        check!(theme_size);
        check!(plugin_layout_size);
        check!(color_size);
        check!(vec_u8_size);
        check!(option_usize_size);
        check!(audio_buffer_align);
        check!(process_status_align);
        check!(result_normal_disc);
        check!(result_tail_disc);
        check!(result_keepalive_disc);
        check!(rustc_version_hash);
        check!(sample_precision);
        diffs
    }
}

fn discriminant_byte<T>(value: &T) -> u8 {
    // SAFETY: `value: &T` points to a valid `T`, and any `T` has at
    // least its first byte readable (alignment + size > 0). The
    // discriminant of a `#[repr(...)]`-tagged or default-repr enum
    // lives at offset 0, so the first byte is exactly the value the
    // canary wants to compare. For non-enum `T` the byte is whatever
    // the layout puts there - fine, because the canary fields that
    // call this (`result_*_disc`) only pass `ProcessStatus` variants
    // and only compare the result against the matching dylib reading
    // of the same call.
    unsafe { *ptr::from_ref::<T>(value).cast::<u8>() }
}

fn rustc_hash() -> u64 {
    env!("TRUCE_RUSTC_HASH").parse().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Vtable probe
// ---------------------------------------------------------------------------

/// A plugin with known return values for vtable verification.
///
/// The shell creates this via `truce_vtable_probe()`, calls every
/// method, and checks the results. If any method returns the wrong
/// value, the vtable is reordered and the dylib is rejected.
///
/// `last_load_state` is the only mutable cell - `load_state` writes
/// it, `save_state` reads it back. This lets `verify_probe`
/// round-trip a sentinel through the load/save pair to confirm the
/// `load_state` slot isn't swapped with another `&mut self` slot.
#[derive(Default)]
pub struct ProbePlugin {
    last_load_state: RefCell<Vec<u8>>,
}

impl<S: Sample> PluginLogicCore<S> for ProbePlugin {
    fn supports_in_place() -> bool
    where
        Self: Sized,
    {
        false
    }

    fn bus_layouts() -> Vec<truce_core::bus::BusLayout>
    where
        Self: Sized,
    {
        vec![truce_core::bus::BusLayout::stereo()]
    }

    fn reset(&mut self, _sr: f64, _bs: usize) {}

    fn process(
        &mut self,
        _buffer: &mut AudioBuffer<S>,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }

    fn save_state(&self) -> Vec<u8> {
        // If `load_state` wasn't called, return the default sentinel;
        // otherwise echo what was just loaded so verify can check the
        // load/save vtable slots aren't crossed.
        let cached = self.last_load_state.borrow();
        if cached.is_empty() {
            vec![0xCA, 0xFE]
        } else {
            cached.clone()
        }
    }
    fn load_state(&mut self, data: &[u8]) -> Result<(), truce_core::state::StateLoadError> {
        *self.last_load_state.borrow_mut() = data.to_vec();
        Ok(())
    }
    fn state_changed(&mut self) {}
    fn migrate_state(
        _foreign: &truce_core::state::ForeignState,
    ) -> Option<truce_core::state::MigratedState>
    where
        Self: Sized,
    {
        // Receiverless, so it has no vtable slot to probe.
        None
    }
    fn latency(&self) -> u32 {
        0xAAAA
    }
    fn tail(&self) -> u32 {
        0xBBBB
    }
}

/// Verify a probe plugin returns the expected values.
///
/// Coverage notes: methods exercised, in source-declaration order:
/// `latency`, `tail`, `save_state` (default path), then `load_state` +
/// `save_state` (echo path). 4 of `PluginLogicCore`'s 8 instance
/// methods covered. The four not exercised (`reset`, `process`,
/// `state_changed`, `editor`) would require constructing an
/// `AudioBuffer` / opening a real window mock, heavyweight enough to
/// outweigh the marginal vtable-reorder detection benefit.
/// (Trait-object dispatch goes through a vtable whose slot order is
/// rustc-internal and not stable; we don't depend on a particular
/// layout. The goal here is just to call enough of the surface that
/// any ABI-affecting reshuffle is likely to land on a method we *do*
/// exercise.)
///
/// # Errors
///
/// Returns `Err(ProbeError)` on the first canary value that failed
/// to round-trip. Each variant pins which trait method drifted so
/// callers can pattern-match.
#[cfg(feature = "shell")]
pub fn verify_probe<S: Sample>(probe: &mut dyn PluginLogicCore<S>) -> Result<(), ProbeError> {
    if probe.latency() != 0xAAAA {
        return Err(ProbeError::Latency {
            expected: 0xAAAA,
            actual: probe.latency(),
        });
    }
    if probe.tail() != 0xBBBB {
        return Err(ProbeError::Tail {
            expected: 0xBBBB,
            actual: probe.tail(),
        });
    }
    if probe.save_state() != vec![0xCA, 0xFE] {
        return Err(ProbeError::SaveStateDefault);
    }
    // Round-trip a sentinel through load_state → save_state to confirm
    // the load slot isn't swapped with another `&mut self` slot.
    let sentinel = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
    probe
        .load_state(&sentinel)
        .map_err(ProbeError::LoadStateFailed)?;
    if probe.save_state() != sentinel {
        return Err(ProbeError::LoadSaveRoundTrip);
    }
    Ok(())
}

/// Why a vtable probe rejected a candidate dylib. Each variant
/// names the trait method whose canary value drifted; the loader
/// logs the `Display` form and refuses the load.
#[cfg(feature = "shell")]
#[derive(Debug)]
pub enum ProbeError {
    /// `PluginLogicCore::latency` didn't return the canary value.
    Latency { expected: u32, actual: u32 },
    /// `PluginLogicCore::tail` didn't return the canary value.
    Tail { expected: u32, actual: u32 },
    /// `PluginLogicCore::save_state` default path didn't return
    /// the canary `[0xCA, 0xFE]`.
    SaveStateDefault,
    /// `PluginLogicCore::load_state` itself failed (returned `Err`)
    /// for the canary sentinel.
    LoadStateFailed(truce_core::state::StateLoadError),
    /// `load_state` + `save_state` together didn't echo the
    /// sentinel back - the two `&mut self` slots are crossed.
    LoadSaveRoundTrip,
}

#[cfg(feature = "shell")]
impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Latency { expected, actual } => {
                write!(f, "latency: expected 0x{expected:X}, got 0x{actual:X}")
            }
            Self::Tail { expected, actual } => {
                write!(f, "tail: expected 0x{expected:X}, got 0x{actual:X}")
            }
            Self::SaveStateDefault => f.write_str("save_state (default): expected [0xCA, 0xFE]"),
            Self::LoadStateFailed(e) => write!(f, "load_state probe: {e}"),
            Self::LoadSaveRoundTrip => f.write_str("load_state/save_state round-trip mismatch"),
        }
    }
}

#[cfg(feature = "shell")]
impl std::error::Error for ProbeError {}

#[cfg(test)]
mod tests {
    use super::{ABI_EPOCH, AbiCanary};

    #[test]
    fn same_build_matches_itself() {
        let a = AbiCanary::current::<f32>();
        let b = AbiCanary::current::<f32>();
        assert!(a.matches(&b));
    }

    #[test]
    fn epoch_mismatch_fails_and_names_the_field() {
        // A layout change that lands in former padding leaves every
        // size field identical - the epoch is the only tripwire.
        let a = AbiCanary::current::<f32>();
        let mut b = AbiCanary::current::<f32>();
        b.abi_epoch = ABI_EPOCH - 1;
        assert!(!a.matches(&b));
        assert!(a.diff_report(&b).contains("abi_epoch"));
    }

    #[test]
    fn precision_mismatch_fails() {
        let a = AbiCanary::current::<f32>();
        let b = AbiCanary::current::<f64>();
        assert!(!a.matches(&b));
    }
}
