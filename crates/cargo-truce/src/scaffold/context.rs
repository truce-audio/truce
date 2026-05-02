//! Template-rendering contexts. Each one is a `Serialize`-shaped
//! flat record matching the field names the corresponding
//! `.tpl` file references.
//!
//! Kept private to the `scaffold` module — only the `Scaffolder`
//! constructs these. Public callers work with `PluginSpec` / the
//! flag inputs in `spec.rs` instead.

use serde::Serialize;

use super::case::to_pascal_case;
use super::kind::PluginKind;
use super::spec::{DepForm, FeatureSet, PluginSpec, VendorInfo};

const REPO_URL: &str = "https://github.com/truce-audio/truce";

// ---------------------------------------------------------------------------
// PluginContext — fields the per-plugin templates (Cargo.toml,
// build.rs, src/lib.rs, src/main.rs) reference.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct PluginContext {
    pub crate_name: String,
    pub crate_lib: String,
    pub struct_name: String,
    pub upper_name: String,
    pub tag: String,

    pub is_workspace: bool,
    pub has_standalone: bool,
    pub is_effect: bool,

    pub default_label: &'static str,
    pub default_features: &'static str,
    pub dep_args: String,

    pub params_struct: String,
    pub process_body: &'static str,
    pub layout_knob: &'static str,
    pub plugin_macro: String,
    pub test_body: &'static str,
}

impl PluginContext {
    pub fn new(
        crate_name: &str,
        kind: PluginKind,
        dep_form: DepForm,
        features: FeatureSet,
        tag: &str,
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
            tag: tag.to_string(),
            is_workspace,
            has_standalone,
            is_effect: kind.is_effect(),
            default_label: default_label(features),
            default_features: default_features(features),
            dep_args: dep_args(dep_form, tag),
            params_struct: kind.params_struct(&struct_name),
            process_body: kind.process_body(),
            layout_knob: kind.layout_knob(),
            plugin_macro: kind.plugin_macro(&struct_name),
            test_body: kind.test_body(),
            struct_name,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceContext — fields the workspace root Cargo.toml.tpl needs.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct WorkspaceContext {
    pub members: Vec<String>,
    pub tag: String,
    pub has_standalone: bool,
}

impl WorkspaceContext {
    pub fn new(plugins: &[PluginSpec], features: FeatureSet, tag: &str) -> Self {
        Self {
            members: plugins
                .iter()
                .map(|p| format!("plugins/{}", p.name))
                .collect(),
            tag: tag.to_string(),
            has_standalone: features.standalone,
        }
    }
}

// ---------------------------------------------------------------------------
// TruceTomlContext — fields truce.toml.tpl needs.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct TruceTomlContext {
    pub vendor_name: String,
    pub vendor_id: String,
    pub vendor_fourcc: String,
    pub plugins: Vec<TruceTomlPlugin>,
}

#[derive(Serialize)]
pub(crate) struct TruceTomlPlugin {
    pub display: String,
    pub bundle_id: String,
    pub crate_name: String,
    pub category: &'static str,
    pub fourcc: String,
    pub au_tag: &'static str,
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
                }
            })
            .collect();
        Self {
            vendor_name: vendor.name.clone(),
            vendor_id: vendor.id.clone(),
            vendor_fourcc: super::fourcc::to_fourcc(&vendor.name),
            plugins: entries,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers — pure functions producing the precomputed string fields
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

fn dep_args(dep_form: DepForm, tag: &str) -> String {
    match dep_form {
        DepForm::GitTag => format!(r#"git = "{REPO_URL}", tag = "{tag}""#),
        DepForm::Workspace => "workspace = true".to_string(),
    }
}
