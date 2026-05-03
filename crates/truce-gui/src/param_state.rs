//! Cross-backend `ParamState` wrapper around `EditorContext`.
//!
//! Both truce-egui and truce-slint re-export this type. It bundles
//! the begin-edit / set-param / end-edit gesture protocol into a
//! single ergonomic surface that immediate-mode UIs (egui) and
//! callback-driven UIs (slint) can both consume.
//!
//! truce-iced uses its own `ParamState` because iced's
//! `Application::update` flow funnels parameter changes through a
//! `Message` enum rather than direct context calls; the iced
//! `ParamState` additionally caches per-tick values to avoid atomic
//! loads inside the canvas program closures.

use std::sync::Arc;

use truce_core::editor::{ClosureBridge, EditorContext};
use truce_params::Params;

/// Bridge between truce's `EditorContext` and a host UI toolkit.
///
/// Provides read/write access to parameter values with proper
/// automation-gesture handling (`begin_edit` / `set_param` / `end_edit`).
///
/// `Clone` is a ref-count bump on the underlying `EditorContext` (which
/// itself wraps an `Arc<dyn EditorBridge>`); slint callbacks rely on it.
#[derive(Clone)]
pub struct ParamState {
    ctx: EditorContext,
}

impl ParamState {
    /// Wrap an `EditorContext` for widget consumption.
    pub fn new(ctx: EditorContext) -> Self {
        Self { ctx }
    }

    /// Get a parameter's normalized value (0.0–1.0).
    pub fn get(&self, id: impl Into<u32>) -> f64 {
        self.ctx.get_param(id.into())
    }

    /// Get a parameter's plain value (in its native range).
    pub fn get_plain(&self, id: impl Into<u32>) -> f64 {
        self.ctx.get_param_plain(id.into())
    }

    /// Get a parameter's formatted display string.
    pub fn format(&self, id: impl Into<u32>) -> String {
        self.ctx.format_param(id.into())
    }

    /// Begin + set + end in one shot (for clicks, toggles, single-shot
    /// sliders that don't gesture across frames).
    pub fn set_immediate(&self, id: impl Into<u32>, normalized: f64) {
        let id = id.into();
        self.ctx.begin_edit(id);
        self.ctx.set_param(id, normalized);
        self.ctx.end_edit(id);
    }

    /// Begin a drag gesture (call once on mouse-down).
    pub fn begin_gesture(&self, id: impl Into<u32>) {
        self.ctx.begin_edit(id.into());
    }

    /// Update during a drag gesture.
    pub fn set_value(&self, id: impl Into<u32>, normalized: f64) {
        self.ctx.set_param(id.into(), normalized);
    }

    /// End a drag gesture (call once on mouse-up).
    pub fn end_gesture(&self, id: impl Into<u32>) {
        self.ctx.end_edit(id.into());
    }

    /// Read a meter value (0.0–1.0).
    pub fn meter(&self, id: impl Into<u32>) -> f32 {
        self.ctx.get_meter(id.into())
    }

    /// Request a resize from the host. Returns true if accepted.
    pub fn request_resize(&self, width: u32, height: u32) -> bool {
        self.ctx.request_resize(width, height)
    }

    /// Access the underlying `EditorContext` (for `StateBinding::new()`).
    pub fn context(&self) -> &EditorContext {
        &self.ctx
    }

    /// Read custom plugin state bytes (from `save_state()`).
    pub fn get_state(&self) -> Vec<u8> {
        self.ctx.get_state()
    }

    /// Write custom state back to the plugin (calls `load_state()`).
    pub fn set_state(&self, data: Vec<u8>) {
        self.ctx.set_state(data);
    }

    /// Build a `ParamState` backed by parameter defaults — used by
    /// snapshot tests so transport-aware widgets can render without a
    /// live host. Production paths construct the `EditorContext` from
    /// the host's own transport / param wiring.
    pub fn from_params(params: Arc<dyn Params>) -> Self {
        let p1 = params.clone();
        let p2 = params.clone();
        let p3 = params.clone();
        let transport = truce_core::events::TransportInfo::for_screenshot();
        Self {
            ctx: EditorContext::from_closures(ClosureBridge {
                begin_edit: Box::new(|_| {}),
                set_param: Box::new(|_, _| {}),
                end_edit: Box::new(|_| {}),
                request_resize: Box::new(|_, _| false),
                get_param: Box::new(move |id| p1.get_normalized(id).unwrap_or(0.5)),
                get_param_plain: Box::new(move |id| p2.get_plain(id).unwrap_or(0.0)),
                format_param: Box::new(move |id| {
                    let plain = p3.get_plain(id).unwrap_or(0.0);
                    p3.format_value(id, plain)
                        .unwrap_or_else(|| format!("{plain:.2}"))
                }),
                get_meter: Box::new(|_| 0.0),
                get_state: Box::new(Vec::new),
                set_state: Box::new(|_| {}),
                transport: Box::new(move || Some(transport.clone())),
            }),
        }
    }
}
