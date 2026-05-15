//! Platform-agnostic render core shared by the baseview-driven editor
//! (`editor`) and the iOS UIView-driven editor (`editor_ios`).
//!
//! Owns two pieces that have nothing to do with the host window
//! system: the `EditorSnapshotClosures` builder (translates an
//! `Arc<P> + Option<PluginContext>` into a `'static`-lived snapshot
//! the widget tree can read) and `render_widgets` (drives the
//! `widgets::draw` call). Lifted out of `editor.rs` so the iOS path
//! doesn't have to duplicate ~110 lines of closure scaffolding.

use std::sync::Arc;

use truce_core::Float;
use truce_core::editor::{PluginContext, PluginContextReadF32};
use truce_gui_types::RenderBackend;
use truce_gui_types::interaction::InteractionState;
use truce_gui_types::layout::Layout;
use truce_gui_types::snapshot::ParamSnapshot;
use truce_gui_types::theme::Theme;
use truce_gui_types::widgets::{self, WidgetType};
use truce_params::Params;

/// Owned `'static` closures that back a `ParamSnapshot` for the current
/// frame. Each closure captures an `Arc` of the params / context, so the
/// struct can live across a separate `&mut self.interaction` borrow.
pub(crate) struct EditorSnapshotClosures {
    pub get_param: Box<dyn Fn(u32) -> f32>,
    pub get_param_plain: Box<dyn Fn(u32) -> f32>,
    pub format_param: Box<dyn Fn(u32) -> String>,
    pub get_meter: Box<dyn Fn(u32) -> f32>,
    pub get_options: Box<dyn Fn(u32) -> Vec<String>>,
    pub default_normalized: Box<dyn Fn(u32) -> f32>,
    pub next_discrete_normalized: Box<dyn Fn(u32) -> f32>,
    pub param_name: Box<dyn Fn(u32) -> String>,
    pub widget_type: Box<dyn Fn(u32) -> WidgetType>,
}

impl EditorSnapshotClosures {
    pub fn as_snapshot(&self) -> ParamSnapshot<'_> {
        ParamSnapshot {
            get_param: &*self.get_param,
            get_param_plain: &*self.get_param_plain,
            format_param: &*self.format_param,
            get_meter: &*self.get_meter,
            get_options: &*self.get_options,
            default_normalized: &*self.default_normalized,
            next_discrete_normalized: &*self.next_discrete_normalized,
            param_name: &*self.param_name,
            widget_type: &*self.widget_type,
        }
    }
}

/// Build the snapshot closures from a plugin's params and (optional)
/// `PluginContext`. With `Some(ctx)` reads route through the host
/// bridge (so editor edits show up in the host's automation); without
/// it the closures fall back to direct `Params::get_*` reads, which
/// is the path the standalone host and unit tests take.
pub(crate) fn build_snapshot_closures<P: Params + 'static>(
    params: &Arc<P>,
    context: Option<&PluginContext>,
) -> EditorSnapshotClosures {
    let ctx = context.cloned();
    let p = Arc::clone(params);
    let p_get = Arc::clone(&p);
    let p_get_plain = Arc::clone(&p);
    let p_fmt = Arc::clone(&p);
    let p_opts = Arc::clone(&p);
    let p_default = Arc::clone(&p);
    let p_next = Arc::clone(&p);
    let p_name = Arc::clone(&p);
    let p_wtype = Arc::clone(&p);

    let get_param: Box<dyn Fn(u32) -> f32> = match &ctx {
        Some(c) => {
            let c = c.clone();
            Box::new(move |id| c.get_param(id))
        }
        None => Box::new(move |id| p_get.get_normalized(id).unwrap_or(0.0).to_f32()),
    };
    let get_param_plain: Box<dyn Fn(u32) -> f32> = match &ctx {
        Some(c) => {
            let c = c.clone();
            Box::new(move |id| c.get_param_plain(id))
        }
        None => Box::new(move |id| p_get_plain.get_plain(id).unwrap_or(0.0).to_f32()),
    };
    let format_param: Box<dyn Fn(u32) -> String> = match &ctx {
        Some(c) => {
            let c = c.clone();
            Box::new(move |id| c.format_param(id))
        }
        None => Box::new(move |id| {
            let v = p_fmt.get_plain(id).unwrap_or(0.0);
            p_fmt
                .format_value(id, v)
                .unwrap_or_else(|| format!("{v:.1}"))
        }),
    };
    let get_meter: Box<dyn Fn(u32) -> f32> = match &ctx {
        Some(c) => {
            let c = c.clone();
            Box::new(move |id| c.get_meter(id))
        }
        None => Box::new(move |_| 0.0),
    };
    let get_options: Box<dyn Fn(u32) -> Vec<String>> = Box::new(move |id| {
        let Some(info) = p_opts.param_infos().iter().find(|i| i.id == id).copied() else {
            return Vec::new();
        };
        let count = info.range.step_count_usize() + 1;
        (0..count)
            .map(|i| {
                let norm = truce_core::cast::discrete_norm(i, count);
                let plain = info.range.denormalize(norm);
                p_opts
                    .format_value(id, plain)
                    .unwrap_or_else(|| format!("{plain:.0}"))
            })
            .collect()
    });
    let default_normalized: Box<dyn Fn(u32) -> f32> = Box::new(move |id| {
        p_default
            .param_infos()
            .iter()
            .find(|i| i.id == id)
            .map_or(0.0, |info| {
                f32::from_f64(info.range.normalize(info.default_plain))
            })
    });
    let next_discrete_normalized: Box<dyn Fn(u32) -> f32> = Box::new(move |id| {
        let Some(info) = p_next.param_infos().iter().find(|i| i.id == id).copied() else {
            return 0.0;
        };
        let plain = p_next.get_plain(id).unwrap_or(0.0);
        let max = info.range.max();
        let next = if plain >= max { 0.0 } else { plain + 1.0 };
        f32::from_f64(info.range.normalize(next))
    });
    let param_name: Box<dyn Fn(u32) -> String> = Box::new(move |id| {
        p_name
            .param_infos()
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.name.to_string())
            .unwrap_or_default()
    });
    let widget_type: Box<dyn Fn(u32) -> WidgetType> = Box::new(move |id| {
        let info = p_wtype.param_infos().iter().find(|i| i.id == id).copied();
        match info.as_ref().map(|i| &i.range) {
            Some(truce_params::ParamRange::Discrete { min: 0, max: 1 }) => WidgetType::Toggle,
            Some(truce_params::ParamRange::Enum { .. }) => WidgetType::Selector,
            _ => WidgetType::Knob,
        }
    });

    EditorSnapshotClosures {
        get_param,
        get_param_plain,
        format_param,
        get_meter,
        get_options,
        default_normalized,
        next_discrete_normalized,
        param_name,
        widget_type,
    }
}

/// Rasterize the full editor into `backend`. Single entry point both
/// `editor.rs` and `editor_ios.rs` delegate to so the clear +
/// dispatch lives in one place.
pub(crate) fn render_widgets(
    layout: &Layout,
    theme: &Theme,
    interaction: &mut InteractionState,
    snapshot: &ParamSnapshot<'_>,
    backend: &mut dyn RenderBackend,
) {
    backend.clear(theme.background);
    widgets::draw(backend, layout, theme, snapshot, interaction);
}
