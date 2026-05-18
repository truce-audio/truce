//! Filesystem layout for scaffold output. Encapsulates every path
//! the `Scaffolder` writes so adding a new emitted file is one
//! method here + one render-and-write call in the driver.

use std::path::{Path, PathBuf};

pub struct ProjectLayout {
    /// Directory the per-plugin files live in. For single-mode
    /// scaffolds this IS the project root; for workspace-mode
    /// plugins it's `<root>/plugins/<name>/`.
    plugin_root: PathBuf,
}

impl ProjectLayout {
    /// Single-mode plugin: project root and plugin root are one and
    /// the same.
    pub fn single(root: impl Into<PathBuf>) -> Self {
        Self {
            plugin_root: root.into(),
        }
    }

    /// A plugin crate inside a workspace scaffold.
    /// `<workspace_root>/plugins/<plugin_name>/`.
    pub fn workspace_plugin(workspace_root: &Path, plugin_name: &str) -> Self {
        Self {
            plugin_root: workspace_root.join("plugins").join(plugin_name),
        }
    }

    pub fn src_dir(&self) -> PathBuf {
        self.plugin_root.join("src")
    }
    pub fn cargo_dir(&self) -> PathBuf {
        self.plugin_root.join(".cargo")
    }
    pub fn cargo_toml(&self) -> PathBuf {
        self.plugin_root.join("Cargo.toml")
    }
    pub fn lib_rs(&self) -> PathBuf {
        self.plugin_root.join("src/lib.rs")
    }
    pub fn main_rs(&self) -> PathBuf {
        self.plugin_root.join("src/main.rs")
    }
    pub fn gitignore(&self) -> PathBuf {
        self.plugin_root.join(".gitignore")
    }
    pub fn cargo_config(&self) -> PathBuf {
        self.plugin_root.join(".cargo/config.toml")
    }
    pub fn truce_toml(&self) -> PathBuf {
        self.plugin_root.join("truce.toml")
    }
}

/// Workspace-root paths - `Cargo.toml`, `truce.toml`, `.gitignore`,
/// `.cargo/config.toml` at the root of a `--workspace` scaffold.
pub struct WorkspaceLayout {
    root: PathBuf,
}

impl WorkspaceLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn cargo_dir(&self) -> PathBuf {
        self.root.join(".cargo")
    }
    pub fn cargo_toml(&self) -> PathBuf {
        self.root.join("Cargo.toml")
    }
    pub fn truce_toml(&self) -> PathBuf {
        self.root.join("truce.toml")
    }
    pub fn gitignore(&self) -> PathBuf {
        self.root.join(".gitignore")
    }
    pub fn cargo_config(&self) -> PathBuf {
        self.root.join(".cargo/config.toml")
    }
}
