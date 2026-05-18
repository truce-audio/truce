//! ABI canary - runtime verification that shell and dylib have
//! compatible type layouts and vtable ordering.

use std::cell::RefCell;
use std::mem::{align_of, size_of};
use std::ptr;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo as Transport};
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_gui::PluginLogicCore;
use truce_gui::interaction::WidgetRegion;
use truce_gui::layout::GridLayout;
use truce_gui::render::RenderBackend;
use truce_gui::theme::{Color, Theme};
use truce_params::sample::Sample;

/// ABI fingerprint. Compared between shell and dylib before loading.
///
/// This is the ONE `#[repr(C)]` type in the system - it's the
/// bootstrap verification struct that makes everything else safe.
#[repr(C)]
pub struct AbiCanary {
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
    /// `truce_abi_canary` export does the same with its own (from the
    /// prelude alias). The two are compared at load time.
    #[must_use]
    pub fn current<S: truce_params::sample::Sample>() -> Self {
        // 8× sizeof gives us 32 for f32 / 64 for f64; the cast to u8
        // can't overflow for any plausible sample type.
        #[allow(clippy::cast_possible_truncation)]
        let sample_precision = (size_of::<S>() * 8) as u8;
        Self {
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
    fn latency(&self) -> u32 {
        0xAAAA
    }
    fn tail(&self) -> u32 {
        0xBBBB
    }

    fn render(&self, _backend: &mut dyn RenderBackend) {}

    fn uses_custom_render(&self) -> bool {
        true
    }

    fn layout(&self) -> GridLayout {
        let mut gl = GridLayout::build(vec![]);
        gl.width = 0xDEAD;
        gl.height = 0xBEEF;
        gl
    }

    fn hit_test(&self, _w: &[WidgetRegion], _x: f32, _y: f32) -> Option<usize> {
        Some(42)
    }

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        None
    }
}

/// Verify a probe plugin returns the expected values.
///
/// Coverage notes - methods exercised, in source-declaration order:
/// `latency`, `tail`, `layout`, `hit_test`, `save_state` (default
/// path), `uses_custom_render`, `custom_editor`, then `load_state` +
/// `save_state` (echo path). 8 of 11 trait methods covered. The three
/// not exercised - `reset`, `process`, `render` - would require
/// constructing an `AudioBuffer` / `RenderBackend` mock, which is
/// heavyweight enough to outweigh the marginal vtable-reorder
/// detection benefit. (Trait-object dispatch goes through a vtable
/// whose slot order is rustc-internal and not stable; we don't depend
/// on a particular layout - the goal here is just to call enough of
/// the surface that any ABI-affecting reshuffle is likely to land on
/// a method we *do* exercise.)
///
/// # Errors
///
/// Returns `Err(ProbeError)` on the first canary value that failed
/// to round-trip. Each variant pins which trait method drifted so
/// callers can pattern-match (today only the in-crate loader logs
/// the `Display`, but a future ABI-divergence dashboard could
/// group failures by variant).
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
    let layout = probe.layout();
    if layout.width != 0xDEAD || layout.height != 0xBEEF {
        return Err(ProbeError::Layout {
            width: layout.width,
            height: layout.height,
        });
    }
    if probe.hit_test(&[], 0.0, 0.0) != Some(42) {
        return Err(ProbeError::HitTest);
    }
    if probe.save_state() != vec![0xCA, 0xFE] {
        return Err(ProbeError::SaveStateDefault);
    }
    if !probe.uses_custom_render() {
        return Err(ProbeError::UsesCustomRender);
    }
    if probe.custom_editor().is_some() {
        return Err(ProbeError::CustomEditor);
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
    /// `PluginLogicCore::layout` returned a `GridLayout` whose
    /// width/height didn't match the canary's `0xDEAD × 0xBEEF`.
    Layout { width: u32, height: u32 },
    /// `PluginLogicCore::hit_test` didn't return `Some(42)`.
    HitTest,
    /// `PluginLogicCore::save_state` default path didn't return
    /// the canary `[0xCA, 0xFE]`.
    SaveStateDefault,
    /// `PluginLogicCore::uses_custom_render` returned `false` where
    /// the canary expects `true`.
    UsesCustomRender,
    /// `PluginLogicCore::custom_editor` returned `Some` where the
    /// canary expects `None`.
    CustomEditor,
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
            Self::Layout { width, height } => write!(
                f,
                "layout: expected 0xDEAD×0xBEEF, got 0x{width:X}×0x{height:X}"
            ),
            Self::HitTest => f.write_str("hit_test: expected Some(42)"),
            Self::SaveStateDefault => f.write_str("save_state (default): expected [0xCA, 0xFE]"),
            Self::UsesCustomRender => f.write_str("uses_custom_render: expected true"),
            Self::CustomEditor => f.write_str("custom_editor: expected None"),
            Self::LoadStateFailed(e) => write!(f, "load_state probe: {e}"),
            Self::LoadSaveRoundTrip => f.write_str("load_state/save_state round-trip mismatch"),
        }
    }
}

#[cfg(feature = "shell")]
impl std::error::Error for ProbeError {}
