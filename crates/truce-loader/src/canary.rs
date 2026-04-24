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

    pub fn matches(&self, other: &Self) -> bool {
        self.trait_object_size == other.trait_object_size
            && self.audio_buffer_size == other.audio_buffer_size
            && self.process_context_size == other.process_context_size
            && self.process_status_size == other.process_status_size
            && self.event_size == other.event_size
            && self.event_body_size == other.event_body_size
            && self.transport_size == other.transport_size
            && self.widget_region_size == other.widget_region_size
            && self.theme_size == other.theme_size
            && self.plugin_layout_size == other.plugin_layout_size
            && self.color_size == other.color_size
            && self.vec_u8_size == other.vec_u8_size
            && self.option_usize_size == other.option_usize_size
            && self.audio_buffer_align == other.audio_buffer_align
            && self.process_status_align == other.process_status_align
            && self.result_normal_disc == other.result_normal_disc
            && self.result_tail_disc == other.result_tail_disc
            && self.result_keepalive_disc == other.result_keepalive_disc
            && self.rustc_version_hash == other.rustc_version_hash
    }

    pub fn diff_report(&self, other: &Self) -> String {
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
        if diffs.is_empty() {
            "no differences".into()
        } else {
            format!("ABI mismatches:\n{}", diffs.join("\n"))
        }
    }
}

fn discriminant_byte<T>(value: &T) -> u8 {
    unsafe { *(value as *const T as *const u8) }
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
pub struct ProbePlugin;

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

    fn layout(&self) -> truce_gui::layout::GridLayout {
        let mut gl = truce_gui::layout::GridLayout::build("", "", 1, 80.0, vec![]);
        gl.width = 0xDEAD;
        gl.height = 0xBEEF;
        gl
    }

    fn hit_test(&self, _w: &[WidgetRegion], _x: f32, _y: f32) -> Option<usize> {
        Some(42)
    }

    fn save_state(&self) -> Vec<u8> {
        vec![0xCA, 0xFE]
    }
    fn load_state(&mut self, _data: &[u8]) {}
    fn latency(&self) -> u32 {
        0xAAAA
    }
    fn tail(&self) -> u32 {
        0xBBBB
    }
}

/// Verify a probe plugin returns the expected values.
pub fn verify_probe(probe: &dyn PluginLogic) -> Result<(), String> {
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
        return Err("save_state: expected [0xCA, 0xFE]".into());
    }
    Ok(())
}
