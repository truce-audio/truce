//! Template-rendering contexts. Each one is a `Serialize`-shaped
//! flat record matching the field names the corresponding
//! `.tpl` file references.
//!
//! Kept private to the `scaffold` module - only the `Scaffolder`
//! constructs these. Public callers work with `PluginSpec` / the
//! flag inputs in `spec.rs` instead.

use serde::Serialize;

use super::case::to_pascal_case;
use super::kind::PluginKind;
use super::spec::{DepForm, FeatureSet, PluginSpec, VendorInfo};
use super::statefulness::Statefulness;

const REPO_URL: &str = "https://github.com/truce-audio/truce";

/// Version / registry pins the `Scaffolder` threads into every
/// per-plugin template. Grouped into one argument so the context
/// constructors stay under the argument-count lint.
#[derive(Clone, Copy)]
pub(crate) struct Pins<'a> {
    pub tag: &'a str,
    pub version: &'a str,
    pub use_registry: bool,
}

// ---------------------------------------------------------------------------
// PluginScaffoldingContext - fields the per-plugin templates (Cargo.toml,
// build.rs, src/lib.rs, src/main.rs) reference.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct PluginScaffoldingContext {
    pub crate_name: String,
    pub crate_lib: String,
    pub struct_name: String,
    pub upper_name: String,
    /// `v`-prefixed tag string (e.g. `v0.48.7`). Used by the git+tag
    /// dep form (the default) and by the workspace template's
    /// commented opt-in hints when `use_registry` is false.
    pub tag: String,
    /// Plain semver version (e.g. `0.48.7`). Used by the registry
    /// dep form when `use_registry` is true.
    pub version: String,
    /// Toggles the dep style emitted by the per-plugin Cargo.toml's
    /// commented LV2/AU/AAX hints. Mirrors the same flag on
    /// `WorkspaceContext` so workspace + plugin templates stay in
    /// sync.
    pub use_registry: bool,

    pub is_workspace: bool,
    pub has_standalone: bool,

    pub default_label: &'static str,
    pub default_features: &'static str,
    pub dep_args: String,

    pub params_struct: String,
    /// Leaf trait the `impl ... for` header names (`PurePluginLogic`
    /// or `PluginLogic`).
    pub impl_trait: &'static str,
    /// Descriptor doc comment emitted above the descriptor struct.
    pub descriptor_block: String,
    /// The descriptor struct declaration: a unit struct for a pure
    /// plugin, or a `#[derive(Default)]` struct that is its own DSP state
    /// (`type DspState = Self`) for a stateful one.
    pub struct_decl: String,
    /// `type DspState = ...;` line inside the impl block; empty for a
    /// pure plugin.
    pub dsp_state_type: String,
    pub process_body: String,
    pub bus_layouts_method: &'static str,
    pub layout_knob: &'static str,
    pub plugin_macro: String,
}

impl PluginScaffoldingContext {
    pub fn new(
        crate_name: &str,
        kind: PluginKind,
        statefulness: Statefulness,
        dep_form: DepForm,
        features: FeatureSet,
        pins: Pins<'_>,
    ) -> Self {
        let struct_name = to_pascal_case(crate_name);
        let crate_lib = crate_name.replace('-', "_");
        let upper_name = struct_name.to_uppercase();
        let is_workspace = dep_form == DepForm::Workspace;
        let has_standalone = features.standalone;
        Self {
            crate_name: crate_name.to_string(),
            crate_lib,
            upper_name,
            tag: pins.tag.to_string(),
            version: pins.version.to_string(),
            use_registry: pins.use_registry,
            is_workspace,
            has_standalone,
            default_label: default_label(features),
            default_features: default_features(features),
            dep_args: dep_args(dep_form, pins.tag, pins.version),
            params_struct: kind.params_struct(&struct_name),
            impl_trait: statefulness.impl_trait(),
            descriptor_block: statefulness.descriptor_block(),
            struct_decl: statefulness.struct_decl(&struct_name),
            dsp_state_type: statefulness.dsp_state_type(),
            process_body: statefulness.wrap_process(kind.process_body()),
            bus_layouts_method: kind.bus_layouts_method(),
            layout_knob: kind.layout_knob(),
            plugin_macro: kind.plugin_macro(&struct_name),
            struct_name,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceContext - fields the workspace root Cargo.toml.tpl needs.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct WorkspaceContext {
    pub members: Vec<String>,
    pub tag: String,
    pub version: String,
    pub use_registry: bool,
    pub has_standalone: bool,
}

impl WorkspaceContext {
    pub fn new(
        plugins: &[PluginSpec],
        features: FeatureSet,
        tag: &str,
        version: &str,
        use_registry: bool,
    ) -> Self {
        Self {
            members: plugins
                .iter()
                .map(|p| format!("plugins/{}", p.name))
                .collect(),
            tag: tag.to_string(),
            version: version.to_string(),
            use_registry,
            has_standalone: features.standalone,
        }
    }
}

// ---------------------------------------------------------------------------
// TruceTomlContext - fields truce.toml.tpl needs.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct TruceTomlContext {
    pub vendor_name: String,
    pub vendor_id: String,
    pub vendor_fourcc: String,
    pub plugins: Vec<TruceTomlPlugin>,
    /// Suite-installer block emitted only for multi-plugin workspace
    /// scaffolds. `None` collapses the `{{ if suite }}` template guard
    /// so single-plugin scaffolds get a clean `truce.toml` without
    /// boilerplate they don't need.
    pub suite: Option<TruceTomlSuite>,
}

#[derive(Serialize)]
pub(crate) struct TruceTomlPlugin {
    pub display: String,
    pub bundle_id: String,
    pub crate_name: String,
    pub category: &'static str,
    pub fourcc: String,
    pub au_tag: &'static str,
    pub vst3_subcategory: &'static str,
}

#[derive(Serialize)]
pub(crate) struct TruceTomlSuite {
    pub name: String,
    pub bundle_id: String,
}

impl TruceTomlContext {
    pub fn new(
        vendor: &VendorInfo,
        plugins: &[PluginSpec],
        workspace_name: &str,
        fourcc_map: &std::collections::HashMap<String, String>,
        is_workspace: bool,
    ) -> Self {
        let entries = plugins
            .iter()
            .map(|p| {
                let display = to_pascal_case(&p.name);
                let crate_name = if is_workspace {
                    format!("{workspace_name}-{}", p.name)
                } else {
                    p.name.clone()
                };
                TruceTomlPlugin {
                    display,
                    bundle_id: p.name.clone(),
                    crate_name,
                    category: p.kind.category(),
                    fourcc: fourcc_map[&p.name].clone(),
                    au_tag: p.kind.au_tag(),
                    vst3_subcategory: p.kind.vst3_subcategory(),
                }
            })
            .collect();
        // Suite installers wrap multiple plugins. A single-plugin
        // scaffold has nothing to wrap, so don't emit a `[[suite]]`
        // block - the per-plugin installer is exactly what the user
        // wants.
        let suite = (is_workspace && plugins.len() >= 2).then(|| TruceTomlSuite {
            name: to_pascal_case(workspace_name),
            bundle_id: format!("{workspace_name}-suite"),
        });
        Self {
            vendor_name: vendor.name.clone(),
            vendor_id: vendor.id.clone(),
            vendor_fourcc: super::fourcc::to_fourcc(&vendor.name),
            plugins: entries,
            suite,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers - pure functions producing the precomputed string fields
// the templates substitute in.
// ---------------------------------------------------------------------------

fn default_label(features: FeatureSet) -> &'static str {
    if features.standalone {
        "CLAP + VST3 + standalone"
    } else {
        "CLAP + VST3"
    }
}

fn default_features(features: FeatureSet) -> &'static str {
    if features.standalone {
        r#"["clap", "vst3", "standalone"]"#
    } else {
        r#"["clap", "vst3"]"#
    }
}

fn dep_args(dep_form: DepForm, tag: &str, version: &str) -> String {
    match dep_form {
        DepForm::GitTag => format!(r#"git = "{REPO_URL}", tag = "{tag}""#),
        DepForm::Registry => format!(r#"version = "{version}""#),
        DepForm::Workspace => "workspace = true".to_string(),
    }
}
