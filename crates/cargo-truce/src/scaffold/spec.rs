//! Inputs the CLI hands to the `Scaffolder`. Distinct from the
//! template-rendering contexts (`scaffold::context`) so the public
//! surface stays small + serde-free.

use super::{PluginKind, Statefulness};

/// Per-plugin spec - the unit a template consumer thinks in.
pub struct PluginSpec {
    pub name: String,
    pub kind: PluginKind,
    /// Whether the plugin implements `PurePluginLogic` (params-only)
    /// or `PluginLogic` with an explicit DSP-state struct.
    pub statefulness: Statefulness,
}

/// How `[dependencies]` lines are written. Single-mode picks
/// between a crates.io version pin (default) and a git+tag pin
/// (opt-in via `--github`); workspace-mode plugins inherit from
/// the workspace root via `workspace = true`.
#[derive(Clone, Copy, PartialEq)]
pub enum DepForm {
    /// `truce-* = { version = "X.Y" }`. **Default** for
    /// single-mode scaffolds. Resolves against crates.io; caret
    /// semver lets the plugin pick up patch releases without
    /// re-scaffolding.
    Registry,
    /// `truce-* = { git = "...", tag = "vX.Y.Z" }`. Opt-in via
    /// `--github` on `cargo truce new`. Use when scaffolding
    /// against an unreleased truce checkout or otherwise reaching
    /// for the pre-crates.io path.
    GitTag,
    /// `truce-* = { workspace = true }`. Used by plugin crates
    /// inside a `cargo truce new --workspace` scaffold; the
    /// workspace root carries the actual pin (registry or git+tag,
    /// chosen by the same `--github` flag).
    Workspace,
}

/// Per-feature opt-in flags. One boolean per feature so adding a new
/// flag is a straightforward struct extension.
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
    /// Single-mode placeholder; the user is expected to edit
    /// `truce.toml` after scaffolding. Changing these strings will
    /// invalidate the scaffold golden fixtures.
    #[must_use]
    pub fn placeholder() -> Self {
        Self {
            name: "My Company".into(),
            id: "com.mycompany".into(),
        }
    }

    /// Derive a vendor identity from a workspace name when the
    /// user didn't pass `--vendor` / `--vendor-id`.
    #[must_use]
    pub fn derive_from_workspace_name(workspace_name: &str) -> Self {
        Self {
            name: super::case::to_pascal_case(workspace_name),
            id: format!("com.{}", workspace_name.replace('-', "")),
        }
    }
}
