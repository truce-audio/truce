//! Project scaffolding for `cargo truce new`.
//!
//! Two entry points:
//!
//! - [`Scaffolder::single`] — one plugin, project root = plugin
//!   crate root. Used by `cargo truce new <name>`.
//! - [`Scaffolder::workspace`] — N plugins under
//!   `<root>/plugins/<name>/`, with a workspace `Cargo.toml` +
//!   `truce.toml` + `.cargo` at the root. Used by
//!   `cargo truce new <root> --workspace <p1> [p2 …]`.
//!
//! Both paths consume `PluginSpec` + `FeatureSet` + `VendorInfo`
//! and produce the same per-plugin files (lib.rs, build.rs,
//! Cargo.toml, …) via tinytemplate. The single-vs-workspace shape
//! difference is one [`DepForm`] axis on the per-plugin Cargo.toml
//! template.

mod case;
mod context;
mod fourcc;
mod kind;
mod layout;
mod render;
mod spec;

use std::fs;
use std::path::{Path, PathBuf};

use crate::{BoxErr, Res};

use context::{PluginScaffoldingContext, TruceTomlContext, WorkspaceContext};
use layout::{ProjectLayout, WorkspaceLayout};
use render::{Renderer, tpl};

pub use case::to_pascal_case;
pub use fourcc::{resolve_fourccs, to_fourcc};
pub use kind::PluginKind;
pub use spec::{DepForm, FeatureSet, PluginSpec, VendorInfo};

/// Driver that owns the renderer + writes scaffold output to disk.
///
/// One `Scaffolder` per `cargo truce new` invocation. Constructing
/// it builds the tinytemplate registry once (cheap, but the
/// templates are parsed eagerly so we'd rather pay the cost in
/// one place).
pub struct Scaffolder {
    renderer: Renderer,
    tag: String,
}

impl Scaffolder {
    /// Build a fresh scaffolder. The pinned tag is derived from
    /// cargo-truce's own version (`CARGO_PKG_VERSION`) — when the
    /// workspace version bumps, scaffolds automatically follow.
    #[must_use] 
    pub fn new() -> Self {
        Self {
            renderer: Renderer::new(),
            tag: format!("v{}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// Single-mode scaffold — one plugin crate at `<root>/`.
    ///
    /// # Errors
    ///
    /// Propagates any I/O error from writing files under `root`,
    /// fourcc-collision errors from [`resolve_fourccs`], or template
    /// render failures.
    pub fn single(
        &self,
        root: &Path,
        plugin: &PluginSpec,
        features: FeatureSet,
        vendor: &VendorInfo,
    ) -> Res {
        let layout = ProjectLayout::single(root);
        self.write_plugin_files(
            &layout,
            &plugin.name,
            plugin.kind,
            DepForm::GitTag,
            features,
        )?;

        let plugins = std::slice::from_ref(plugin);
        let fourcc_map = resolve_fourccs(plugins).map_err(|e| -> BoxErr { e.into() })?;
        let truce_ctx = TruceTomlContext::new(vendor, plugins, &plugin.name, &fourcc_map, false);
        let truce_path = layout.truce_toml();
        write(
            &truce_path,
            self.renderer.render(tpl::TRUCE_TOML, &truce_ctx),
        )?;

        Ok(())
    }

    /// Workspace-mode scaffold — N plugin crates under
    /// `<root>/plugins/<name>/` plus the workspace root files.
    ///
    /// # Errors
    ///
    /// Propagates any I/O error from writing files under `root`,
    /// fourcc-collision errors from [`resolve_fourccs`], or template
    /// render failures.
    pub fn workspace(
        &self,
        root: &Path,
        workspace_name: &str,
        plugins: &[PluginSpec],
        features: FeatureSet,
        vendor: &VendorInfo,
    ) -> Res {
        let ws_layout = WorkspaceLayout::new(root);

        // Workspace root: Cargo.toml, truce.toml, .gitignore, .cargo/config.toml.
        fs::create_dir_all(ws_layout.cargo_dir())?;

        let ws_ctx = WorkspaceContext::new(plugins, features, &self.tag);
        write(
            &ws_layout.cargo_toml(),
            self.renderer.render(tpl::WORKSPACE_CARGO_TOML, &ws_ctx),
        )?;

        let fourcc_map = resolve_fourccs(plugins).map_err(|e| -> BoxErr { e.into() })?;
        let truce_ctx = TruceTomlContext::new(vendor, plugins, workspace_name, &fourcc_map, true);
        write(
            &ws_layout.truce_toml(),
            self.renderer.render(tpl::TRUCE_TOML, &truce_ctx),
        )?;

        write(
            &ws_layout.gitignore(),
            include_str!("../../templates/scaffold/plugin/.gitignore.tpl").to_string(),
        )?;
        write(
            &ws_layout.cargo_config(),
            include_str!("../../templates/scaffold/plugin/.cargo/config.toml.tpl").to_string(),
        )?;

        // Per-plugin crates.
        for p in plugins {
            let layout = ProjectLayout::workspace_plugin(root, &p.name);
            let crate_name = format!("{workspace_name}-{}", p.name);
            self.write_plugin_files(&layout, &crate_name, p.kind, DepForm::Workspace, features)?;
        }

        Ok(())
    }

    /// Shared per-plugin file emission. Used by both single and
    /// workspace modes — `dep_form` distinguishes the two.
    fn write_plugin_files(
        &self,
        layout: &ProjectLayout,
        crate_name: &str,
        kind: PluginKind,
        dep_form: DepForm,
        features: FeatureSet,
    ) -> Res {
        fs::create_dir_all(layout.src_dir())?;
        fs::create_dir_all(layout.cargo_dir())?;

        let ctx = PluginScaffoldingContext::new(crate_name, kind, dep_form, features, &self.tag);

        write(
            &layout.cargo_toml(),
            self.renderer.render(tpl::PLUGIN_CARGO_TOML, &ctx),
        )?;
        write(
            &layout.build_rs(),
            self.renderer.render(tpl::PLUGIN_BUILD_RS, &ctx),
        )?;
        write(
            &layout.lib_rs(),
            self.renderer.render(tpl::PLUGIN_LIB_RS, &ctx),
        )?;
        if features.standalone {
            write(
                &layout.main_rs(),
                self.renderer.render(tpl::PLUGIN_MAIN_RS, &ctx),
            )?;
        }
        // Single-mode plugins own their own .gitignore + .cargo;
        // workspace-mode plugins inherit those from the workspace
        // root (caller handles), so skip when `dep_form` is
        // `Workspace`.
        if dep_form == DepForm::GitTag {
            write(
                &layout.gitignore(),
                self.renderer.render(tpl::PLUGIN_GITIGNORE, &ctx),
            )?;
            write(
                &layout.cargo_config(),
                self.renderer.render(tpl::PLUGIN_CARGO_CONFIG, &ctx),
            )?;
        }
        Ok(())
    }
}

impl Default for Scaffolder {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapper around `fs::write` that returns the same `Res` shape
/// the rest of cargo-truce uses, with the path threaded into the
/// error so failures are diagnosable.
fn write(path: &PathBuf, content: String) -> Res {
    fs::write(path, content).map_err(|e| {
        let msg: Box<dyn std::error::Error> = format!("write {}: {}", path.display(), e).into();
        msg
    })?;
    Ok(())
}
