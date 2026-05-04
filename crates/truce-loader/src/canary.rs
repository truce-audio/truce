//! ABI canary — runtime verification that shell and dylib have
//! compatible type layouts and vtable ordering.

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo as Transport};
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_gui::interaction::WidgetRegion;
use truce_gui::layout::GridLayout;
use truce_gui::render::RenderBackend;
use truce_gui::theme::{Color, Theme};

use crate::traits::PluginLogic;

/// ABI fingerprint. Compared between shell and dylib before loading.
///
/// This is the ONE `#[repr(C)]` type in the system — it's the
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
}

impl AbiCanary {
    #[must_use] 
    pub fn current() -> Self {
        Self {
            trait_object_size: std::mem::size_of::<*const dyn PluginLogic>() * 2,
            audio_buffer_size: std::mem::size_of::<AudioBuffer>(),
            process_context_size: std::mem::size_of::<ProcessContext>(),
            process_status_size: std::mem::size_of::<ProcessStatus>(),
            event_size: std::mem::size_of::<Event>(),
            event_body_size: std::mem::size_of::<EventBody>(),
            transport_size: std::mem::size_of::<Transport>(),
            widget_region_size: std::mem::size_of::<WidgetRegion>(),
            theme_size: std::mem::size_of::<Theme>(),
            plugin_layout_size: std::mem::size_of::<GridLayout>(),
            color_size: std::mem::size_of::<Color>(),
            vec_u8_size: std::mem::size_of::<Vec<u8>>(),
            option_usize_size: std::mem::size_of::<Option<usize>>(),
            audio_buffer_align: std::mem::align_of::<AudioBuffer>(),
            process_status_align: std::mem::align_of::<ProcessStatus>(),
            result_normal_disc: discriminant_byte(&ProcessStatus::Normal),
            result_tail_disc: discriminant_byte(&ProcessStatus::Tail(0)),
            result_keepalive_disc: discriminant_byte(&ProcessStatus::KeepAlive),
            rustc_version_hash: rustc_hash(),
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
        // Single source of truth — adding a field to AbiCanary means
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
        diffs
    }
}

fn discriminant_byte<T>(value: &T) -> u8 {
    unsafe { *std::ptr::from_ref::<T>(value).cast::<u8>() }
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
/// `last_load_state` is the only mutable cell — `load_state` writes
/// it, `save_state` reads it back. This lets `verify_probe`
/// round-trip a sentinel through the load/save pair to confirm the
/// `load_state` slot isn't swapped with another `&mut self` slot.
pub struct ProbePlugin {
    last_load_state: std::cell::RefCell<Vec<u8>>,
}

impl ProbePlugin {
    #[must_use] 
    pub fn new() -> Self {
        Self {
            last_load_state: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl Default for ProbePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginLogic for ProbePlugin {
    fn reset(&mut self, _sr: f64, _bs: usize) {}

    fn process(
        &mut self,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
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

    fn save_state(&self) -> Vec<u8> {
        // If `load_state` wasn't called, return the audit-default
        // sentinel; otherwise echo what was just loaded so verify can
        // check the load/save vtable slots aren't crossed.
        let cached = self.last_load_state.borrow();
        if cached.is_empty() {
            vec![0xCA, 0xFE]
        } else {
            cached.clone()
        }
    }
    fn load_state(&mut self, data: &[u8]) {
        *self.last_load_state.borrow_mut() = data.to_vec();
    }
    fn latency(&self) -> u32 {
        0xAAAA
    }
    fn tail(&self) -> u32 {
        0xBBBB
    }
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        None
    }
}

/// Verify a probe plugin returns the expected values.
///
/// Coverage notes — methods exercised, in source-declaration order:
/// `latency`, `tail`, `layout`, `hit_test`, `save_state` (default
/// path), `uses_custom_render`, `custom_editor`, then `load_state` +
/// `save_state` (echo path). 8 of 11 trait methods covered. The three
/// not exercised — `reset`, `process`, `render` — would require
/// constructing an `AudioBuffer` / `RenderBackend` mock, which is
/// heavyweight enough to outweigh the marginal vtable-reorder
/// detection benefit. (Trait-object dispatch goes through a vtable
/// whose slot order is rustc-internal and not stable; we don't depend
/// on a particular layout — the goal here is just to call enough of
/// the surface that any ABI-affecting reshuffle is likely to land on
/// a method we *do* exercise.)
///
/// # Errors
///
/// Returns `Err(String)` describing the first canary value that
/// failed to round-trip — distinct messages for `latency`, `tail`,
/// `layout`, `hit_test`, `save_state` (default and echo paths),
/// `uses_custom_render`, `custom_editor`, and `load_state`.
pub fn verify_probe(probe: &mut dyn PluginLogic) -> Result<(), String> {
    if probe.latency() != 0xAAAA {
        return Err(format!(
            "latency: expected 0xAAAA, got 0x{:X}",
            probe.latency()
        ));
    }
    if probe.tail() != 0xBBBB {
        return Err(format!("tail: expected 0xBBBB, got 0x{:X}", probe.tail()));
    }
    let layout = probe.layout();
    if layout.width != 0xDEAD || layout.height != 0xBEEF {
        return Err(format!(
            "layout: expected 0xDEAD×0xBEEF, got 0x{:X}×0x{:X}",
            layout.width, layout.height
        ));
    }
    if probe.hit_test(&[], 0.0, 0.0) != Some(42) {
        return Err("hit_test: expected Some(42)".into());
    }
    if probe.save_state() != vec![0xCA, 0xFE] {
        return Err("save_state (default): expected [0xCA, 0xFE]".into());
    }
    if !probe.uses_custom_render() {
        return Err("uses_custom_render: expected true".into());
    }
    if probe.custom_editor().is_some() {
        return Err("custom_editor: expected None".into());
    }
    // Round-trip a sentinel through load_state → save_state to confirm
    // the load slot isn't swapped with another `&mut self` slot.
    let sentinel = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
    probe.load_state(&sentinel);
    if probe.save_state() != sentinel {
        return Err("load_state/save_state round-trip mismatch".into());
    }
    Ok(())
}
