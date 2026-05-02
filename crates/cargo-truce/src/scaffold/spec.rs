//! Inputs the CLI hands to the `Scaffolder`. Distinct from the
//! template-rendering contexts (`scaffold::context`) so the public
//! surface stays small + serde-free.

use super::PluginKind;

/// Per-plugin spec — the unit a template consumer thinks in.
pub struct PluginSpec {
    pub name: String,
    pub kind: PluginKind,
}

/// How `[dependencies]` lines are written. Single-mode pins each
/// truce dep to a git tag; workspace-mode plugins inherit from the
/// workspace root via `workspace = true`.
#[derive(Clone, Copy, PartialEq)]
pub enum DepForm {
    /// `truce-* = { git = "...", tag = "..." }`. Used when the
    /// scaffolded plugin lives outside any workspace and needs to
    /// reach truce directly.
    GitTag,
    /// `truce-* = { workspace = true }`. Used by plugin crates
    /// inside a `cargo truce new --workspace` scaffold; the workspace
    /// root carries the actual git+tag pin.
    Workspace,
}

/// Per-feature opt-in flags. Booleans for now; if we ever grow a
/// `--minimal` / `--no-vst3` axis it lands here as a new bool.
#[derive(Clone, Copy)]
pub struct FeatureSet {
    pub standalone: bool,
}

/// Vendor identity baked into `truce.toml`.
#[derive(Clone)]
pub struct VendorInfo {
    pub name: String,
    pub id: String,
}

impl VendorInfo {
    /// Single-mode placeholder — the user is expected to edit
    /// `truce.toml` after scaffolding. Kept identical to the
    /// pre-refactor placeholder so existing test fixtures stand.
    pub fn placeholder() -> Self {
        Self {
            name: "My Company".into(),
            id: "com.mycompany".into(),
        }
    }

    /// Derive a vendor identity from a workspace name when the
    /// user didn't pass `--vendor` / `--vendor-id`. Mirrors the
    /// pre-refactor behavior of `scaffold_workspace`.
    pub fn derive_from_workspace_name(workspace_name: &str) -> Self {
        Self {
            name: super::case::to_pascal_case(workspace_name),
            id: format!("com.{}", workspace_name.replace('-', "")),
        }
    }
}
