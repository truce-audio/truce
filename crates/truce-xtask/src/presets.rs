//! Factory-preset loading + per-format emission.
//!
//! Reads `.preset` TOML files from a plugin's `presets/` dir (path
//! configurable via `[[plugin]].presets_dir`), canonicalizes them,
//! and writes per-format preset files during `install` / `package`.
//!
//! Keeps `truce-xtask` as the single author-side surface: each
//! per-format installer (`install_clap` / `install_vst3` / ...) calls
//! `emit_<format>_presets` after staging the bundle, so preset
//! emission follows the same dev/release flag set as format builds.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{PluginDef, Res};
use truce_presets::{clap_preset::ClapPresetFile, read_presets_dir, Preset};

/// Resolve a plugin's directory on disk via `cargo metadata`.
///
/// `cargo metadata --no-deps` reports the manifest path for every
/// workspace package; we strip the trailing `Cargo.toml` to get the
/// crate dir. Works for single-crate workspaces (like `truce-rir`)
/// and multi-plugin ones (the `truce` repo with `examples/*`).
pub(crate) fn plugin_crate_dir(project_root: &Path, crate_name: &str) -> Option<PathBuf> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(project_root.join("Cargo.toml"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let packages = meta.get("packages")?.as_array()?;
    for pkg in packages {
        if pkg.get("name").and_then(|n| n.as_str()) == Some(crate_name) {
            let manifest = pkg.get("manifest_path")?.as_str()?;
            return Path::new(manifest).parent().map(|p| p.to_path_buf());
        }
    }
    None
}

/// Resolve the absolute path of a plugin's preset source directory,
/// honoring an explicit `presets_dir` override or defaulting to
/// `{crate_dir}/presets/`. Returns `None` (with an eprintln trace)
/// if the dir doesn't exist — the common "no factory presets" case
/// shouldn't error out the install.
pub(crate) fn resolve_presets_dir(project_root: &Path, p: &PluginDef) -> Option<PathBuf> {
    let crate_dir = plugin_crate_dir(project_root, &p.crate_name)?;
    let rel = p.presets_dir.as_deref().unwrap_or("presets");
    let dir = crate_dir.join(rel);
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// Parse every `.preset` file in the plugin's preset dir.
pub(crate) fn load_presets(
    project_root: &Path,
    p: &PluginDef,
) -> std::result::Result<Vec<Preset>, crate::BoxErr> {
    match resolve_presets_dir(project_root, p) {
        Some(dir) => read_presets_dir(&dir).map_err(|e| -> crate::BoxErr { Box::new(e) }),
        None => Ok(Vec::new()),
    }
}

/// Emit `.clap-preset` files into the sibling `.presets/` directory
/// for a CLAP install.
///
/// The CLAP bundle on macOS is a single-file dylib, so presets live
/// next to it rather than inside it. Our discovery factory advertises
/// this sibling directory to hosts at runtime.
///
/// Bundle layout after emission:
/// ```text
/// ~/Library/Audio/Plug-Ins/CLAP/Truce Reverb.clap          # the dylib
/// ~/Library/Audio/Plug-Ins/CLAP/Truce Reverb.presets/
///     Halls/
///         cathedral.clap-preset
///         large-hall.clap-preset
///     Rooms/
///         small-room.clap-preset
/// ```
pub(crate) fn emit_clap_presets(
    clap_bundle: &Path,
    plugin_id: &str,
    presets: &[Preset],
) -> Res {
    if presets.is_empty() {
        return Ok(());
    }
    let sidecar = clap_preset_sidecar_dir(clap_bundle);
    // A re-install should replace, not accumulate. Blowing the tree away
    // also drops presets the author deleted since the last install.
    let _ = std::fs::remove_dir_all(&sidecar);
    std::fs::create_dir_all(&sidecar)?;

    for preset in presets {
        let file = ClapPresetFile::from_preset(plugin_id, preset)?;
        let cat = preset.effective_category();
        let dir = if cat.is_empty() {
            sidecar.clone()
        } else {
            sidecar.join(cat)
        };
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.clap-preset", preset.stem()));
        std::fs::write(&path, file.to_toml())?;
    }
    eprintln!(
        "  CLAP presets: {} ({})",
        sidecar.display(),
        presets.len()
    );
    Ok(())
}

/// Sibling dir name for CLAP presets: `Foo.clap` → `Foo.presets`.
pub(crate) fn clap_preset_sidecar_dir(clap_bundle: &Path) -> PathBuf {
    let stem = clap_bundle
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".into());
    clap_bundle
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.presets"))
}
