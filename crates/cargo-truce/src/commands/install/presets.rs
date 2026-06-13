//! Factory-preset emission.
//!
//! Reads the plugin's authored `.preset` TOML library
//! (`truce_build::presets`), renders each preset's canonical state
//! envelope once, and re-envelopes it into every format's native
//! preset file during install:
//!
//! - CLAP: `.trucepreset` containers in the bundle (macOS) or a
//!   `<plugin>.presets/` sibling directory (Windows / Linux, where
//!   the `.clap` is a single file). The CLAP wrapper's discovery
//!   provider declares this directory to the host.
//! - VST3: `.vstpreset` files under `Contents/Resources/Presets/`,
//!   the location hosts scan for in-bundle factory presets.
//! - AU v2: `.aupreset` plists under the OS preset location
//!   (`[~]/Library/Audio/Presets/<Vendor>/<Plugin>/`), which Logic
//!   and `GarageBand` walk.
//! - LV2: `pset:Preset` TTL files inside the `.lv2` bundle plus the
//!   `manifest.ttl` entries pointing at them.
//!
//! The state-envelope hash is derived from the same
//! `truce_build::plugin_id` string `truce::plugin_info!()` bakes into
//! the binary, so emitted blobs load through the exact session-restore
//! path at runtime.

use std::path::{Path, PathBuf};

use crate::install_scope::InstallScope;
use crate::util::fs_ctx;
use crate::{Config, PluginDef, Res};
#[cfg(target_os = "macos")]
use crate::{run_sudo, tmp_manifests};
#[cfg(target_os = "macos")]
use std::ffi::OsStr;

// The `.aupreset` emitters only run from the macOS-gated AU install.
use crate::preset_codec::vstpreset_bytes;
#[cfg(target_os = "macos")]
use crate::preset_codec::{aupreset_xml, fourcc_int};
use truce_utils::preset::{PRESET_FILE_EXT, PresetMeta, write_preset_file};
use truce_utils::{safe_filename, state};

/// One preset ready for per-format emission.
pub(crate) struct EmittablePreset {
    meta: PresetMeta,
    /// Canonical state envelope, already stamped with the plugin's
    /// identity hash.
    blob: Vec<u8>,
    /// Source file stem; per-format files reuse it.
    stem: String,
}

impl EmittablePreset {
    /// Path of this preset relative to a format's preset root:
    /// `<category>/<stem>.<ext>`, flat when the preset has no
    /// category. The source-file stem keeps `.trucepreset` paths
    /// stable for CLAP hosts that persist location URIs.
    fn rel_path(&self, ext: &str) -> PathBuf {
        self.rel(&self.stem, ext)
    }

    /// Like [`Self::rel_path`] but named after the display name - for
    /// the host-facing formats (`.vstpreset`, `.aupreset`) whose
    /// hosts label presets by file name. Library validation rejects
    /// duplicate (category, name) pairs, so the path is unique.
    fn display_rel_path(&self, ext: &str) -> PathBuf {
        self.rel(&safe_filename(&self.meta.name), ext)
    }

    fn rel(&self, file_stem: &str, ext: &str) -> PathBuf {
        let file = format!("{file_stem}.{ext}");
        if self.meta.category.is_empty() {
            PathBuf::from(file)
        } else {
            PathBuf::from(safe_filename(&self.meta.category)).join(file)
        }
    }
}

/// A plugin's parsed factory-preset library.
pub(crate) struct FactoryPresets {
    presets: Vec<EmittablePreset>,
}

/// The plugin's authored-library directory (`presets/` next to the
/// crate, or the `[plugin.presets]` override), whether or not it
/// exists yet. `None` when the crate dir can't be located.
pub(crate) fn authored_presets_dir(root: &Path, p: &PluginDef) -> Option<PathBuf> {
    let manifest = crate::util::locate_plugin_manifest(root, &p.crate_name)?;
    let crate_dir = manifest.parent()?;
    let dir = p
        .presets
        .as_ref()
        .map_or("presets", |c| c.factory_dir.as_str());
    Some(crate_dir.join(dir))
}

/// Reject an unusable `[plugin.presets]` `user_dir` loudly at tool
/// time. The runtime resolver falls back to the default path
/// silently (it can't error mid-host-scan); the CLI is where the
/// author hears about the mistake.
pub(crate) fn validate_user_dir(p: &PluginDef) -> Res {
    if let Some(raw) = p.presets.as_ref().and_then(|c| c.user_dir.as_deref())
        && truce_utils::presets::sanitize_preset_user_dir(raw).is_none()
    {
        return Err(format!(
            "[plugin.presets] user_dir \"{raw}\" is not a usable relative path \
             (needs at least one valid segment; `..` is rejected)"
        )
        .into());
    }
    Ok(())
}

/// Load and canonicalise the plugin's factory presets, stamping
/// missing uuids back into the authored files. Returns `Ok(None)`
/// when the plugin has no preset library: no `[plugin.presets]` in
/// truce.toml and no `presets/` directory next to the crate. An
/// explicit `[plugin.presets]` with a missing directory is an error.
pub(crate) fn load_factory_presets(
    root: &Path,
    p: &PluginDef,
    config: &Config,
) -> Result<Option<FactoryPresets>, crate::CargoTruceError> {
    validate_user_dir(p)?;
    let configured_dir = p.presets.as_ref().map(|c| c.factory_dir.clone());

    let Some(dir) = authored_presets_dir(root, p) else {
        if configured_dir.is_some() {
            return Err(format!(
                "[plugin.presets] set for \"{}\" but its crate dir could not be located",
                p.name
            )
            .into());
        }
        return Ok(None);
    };

    if !dir.is_dir() {
        if configured_dir.is_some() {
            return Err(format!(
                "[plugin.presets] points at {} but it does not exist",
                dir.display()
            )
            .into());
        }
        return Ok(None);
    }

    let annotations = truce_build::presets::read_param_annotations(
        &truce_build::target_dir(root)
            .join("lv2-meta")
            .join(&p.crate_name),
    );
    let names = truce_build::presets::ParamNameMap::from_annotations(&annotations);
    let authored = truce_build::presets::read_presets_dir(&dir, true, Some(&names))?;
    if authored.is_empty() {
        return Ok(None);
    }

    let hash = state::hash_plugin_id(&truce_build::plugin_id(&config.vendor.id, &p.name));
    let presets = authored
        .into_iter()
        .map(|a| EmittablePreset {
            blob: a.state_blob(hash),
            meta: a.meta,
            stem: a.stem,
        })
        .collect();
    Ok(Some(FactoryPresets { presets }))
}

/// Write a preset tree (relative path → bytes) under `dest_root`,
/// staging through a tmp directory + `sudo cp` when the destination
/// is root-owned (macOS system scope).
///
/// `replace` wipes `dest_root` first - right for factory-owned
/// directories inside plugin bundles (a preset deleted from the
/// authored library must not linger across re-installs), wrong for
/// shared locations like `Library/Audio/Presets` where hosts save
/// user presets next to ours.
fn write_tree(
    files: &[(PathBuf, Vec<u8>)],
    dest_root: &Path,
    replace: bool,
    needs_sudo: bool,
    tag: &str,
) -> Res {
    #[cfg(target_os = "macos")]
    if needs_sudo {
        if replace {
            run_sudo("rm", &[OsStr::new("-rf"), dest_root.as_os_str()])?;
        }
        let staging = tmp_manifests().join(format!("presets-{tag}"));
        let _ = std::fs::remove_dir_all(&staging);
        for (rel, bytes) in files {
            let dst = staging.join(rel);
            if let Some(parent) = dst.parent() {
                fs_ctx::create_dir_all(parent)?;
            }
            fs_ctx::write(&dst, bytes)?;
        }
        run_sudo("mkdir", &[OsStr::new("-p"), dest_root.as_os_str()])?;
        // `<staging>/.` copies the staged tree's contents into the
        // existing destination rather than nesting the tmp dir name.
        let staging_contents = staging.join(".");
        run_sudo(
            "cp",
            &[
                OsStr::new("-R"),
                staging_contents.as_os_str(),
                dest_root.as_os_str(),
            ],
        )?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (needs_sudo, tag);

    if replace {
        let _ = std::fs::remove_dir_all(dest_root);
    }
    for (rel, bytes) in files {
        let dst = dest_root.join(rel);
        if let Some(parent) = dst.parent() {
            fs_ctx::create_dir_all(parent)?;
        }
        fs_ctx::write(&dst, bytes)?;
    }
    Ok(())
}

/// Emit a tree of `.trucepreset` containers under `dest_root`. Two
/// consumers: the CLAP factory location (the wrapper's discovery
/// provider declares it to the host) and the AU component's
/// `Contents/Resources/Presets/` (the shim's factory-presets
/// property enumerates it).
pub(crate) fn emit_trucepreset_tree(
    fp: &FactoryPresets,
    dest_root: &Path,
    needs_sudo: bool,
    tag: &str,
) -> Res {
    let files: Vec<_> = fp
        .presets
        .iter()
        .map(|p| {
            (
                p.rel_path(PRESET_FILE_EXT),
                write_preset_file(&p.meta, &p.blob),
            )
        })
        .collect();
    write_tree(&files, dest_root, true, needs_sudo, tag)?;
    crate::log_output(format!(
        "      {} factory presets -> {}",
        files.len(),
        dest_root.display()
    ));
    Ok(())
}

/// Emit `p`'s factory presets into the location a standalone host at
/// `exec_path` resolves at runtime (`truce_standalone::presets::
/// installed_factory_root`): `<App>.app/Contents/Resources/Presets`
/// on macOS, a sibling `<bin>.presets/` directory otherwise. No-op
/// when the plugin ships no authored library. Shared by
/// `cargo truce run` (dev loop) and `stage_standalone` (packaging),
/// so the run-staged binary and the installed app find factory
/// presets through the same path - no `--presets-dir` needed.
pub(crate) fn emit_standalone_factory(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    exec_path: &Path,
) -> Res {
    let Some(fp) = load_factory_presets(root, p, config)? else {
        return Ok(());
    };
    let dest = standalone_factory_root(exec_path);
    emit_trucepreset_tree(&fp, &dest, false, &format!("{}-standalone", p.bundle_id))
}

/// The factory-preset directory for a staged standalone executable,
/// mirroring `installed_factory_root` on the standalone side so both
/// agree without a shared constant across the crate boundary.
fn standalone_factory_root(exec_path: &Path) -> PathBuf {
    // macOS `.app`: .../Contents/MacOS/<bin> -> .../Contents/Resources/Presets
    #[cfg(target_os = "macos")]
    if let Some(macos) = exec_path.parent()
        && macos.file_name() == Some(OsStr::new("MacOS"))
        && let Some(contents) = macos.parent()
    {
        return contents.join("Resources/Presets");
    }
    // Bare binary (Linux / Windows, and the macOS run fallback):
    // sibling `<stem>.presets/`.
    let stem = exec_path.file_stem().map_or_else(
        || "plugin".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    exec_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{stem}.presets"))
}

/// Emit `.vstpreset` files into the OS preset location hosts scan.
/// The VST3 spec defines no in-bundle preset location; the scanned
/// roots are the per-OS directories [`vst3_presets_root`] resolves.
/// On macOS that tree is shared with `.aupreset` files and host-saved
/// user presets, so emission overwrites its own files and never wipes
/// the directory. Hosts match presets to the plugin via the class ID
/// in the file header; the vendor / plugin directory names follow the
/// reported factory vendor and display name for the spec-defined walk.
pub(crate) fn emit_vst3_presets(
    fp: &FactoryPresets,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
) -> Res {
    let Some(presets_root) = vst3_presets_root(scope) else {
        return Err("cannot resolve the VST3 preset directory".into());
    };
    let dest_root = presets_root
        .join(safe_filename(&config.vendor.name))
        .join(safe_filename(resolved_name(
            p.vst3_name.as_deref(),
            &p.name,
        )));
    let cid = state::vst3_cid(&truce_build::plugin_id(&config.vendor.id, &p.name));
    let files: Vec<_> = fp
        .presets
        .iter()
        .map(|pr| {
            (
                pr.display_rel_path("vstpreset"),
                vstpreset_bytes(&cid, &pr.blob),
            )
        })
        .collect();
    write_tree(
        &files,
        &dest_root,
        false,
        scope.needs_sudo(),
        &format!("{}-vst3", p.bundle_id),
    )?;
    crate::log_output(format!(
        "      {} factory presets -> {}",
        files.len(),
        dest_root.display()
    ));
    Ok(())
}

/// The OS root directory VST3 hosts walk for presets, per the
/// Steinberg preset-locations spec:
///
/// - macOS: `[~]/Library/Audio/Presets/`
/// - Windows: `Documents\VST3 Presets\` (user) /
///   `%PROGRAMDATA%\VST3 Presets\` (system)
/// - Linux: `~/.vst3/presets/` (installs are always per-user)
fn vst3_presets_root(scope: InstallScope) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        match scope {
            InstallScope::User => crate::dirs::home_dir().map(|h| h.join("Library/Audio/Presets")),
            InstallScope::System => Some(PathBuf::from("/Library/Audio/Presets")),
        }
    }
    #[cfg(target_os = "windows")]
    {
        match scope {
            InstallScope::User => std::env::var_os("USERPROFILE")
                .map(|p| PathBuf::from(p).join("Documents").join("VST3 Presets")),
            InstallScope::System => {
                std::env::var_os("PROGRAMDATA").map(|p| PathBuf::from(p).join("VST3 Presets"))
            }
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = scope;
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".vst3/presets"))
    }
}

/// Per-format display name: the truce.toml override when set, else
/// the plugin name - the same resolution the format wrappers apply
/// at registration, so preset directories match the name hosts
/// group presets under.
pub(crate) fn resolved_name<'a>(name_override: Option<&'a str>, name: &'a str) -> &'a str {
    match name_override {
        Some(n) if !n.is_empty() => n,
        _ => name,
    }
}

/// Emit `.aupreset` plists into the AU preset location
/// (`[~]/Library/Audio/Presets/<Vendor>/<Plugin>/`), which Logic and
/// `GarageBand` walk on AU instantiation. macOS-only, like the AU
/// install itself.
#[cfg(target_os = "macos")]
pub(crate) fn emit_au_presets(
    fp: &FactoryPresets,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
) -> Res {
    let presets_root = match scope {
        InstallScope::User => {
            let Some(home) = crate::dirs::home_dir() else {
                return Err("cannot resolve home directory for AU presets".into());
            };
            home.join("Library/Audio/Presets")
        }
        InstallScope::System => PathBuf::from("/Library/Audio/Presets"),
    };
    let dest_root = presets_root
        .join(safe_filename(&config.vendor.name))
        .join(safe_filename(resolved_name(p.au_name.as_deref(), &p.name)));

    let au_type = fourcc_int(p.resolved_au_type())?;
    let subtype = fourcc_int(p.resolved_fourcc())?;
    let manufacturer = fourcc_int(&config.vendor.au_manufacturer)?;

    let files: Vec<_> = fp
        .presets
        .iter()
        .map(|pr| {
            (
                pr.display_rel_path("aupreset"),
                aupreset_xml(au_type, subtype, manufacturer, &pr.meta.name, &pr.blob).into_bytes(),
            )
        })
        .collect();
    // Hosts save user presets into the same directory tree - never
    // wipe it, only overwrite our own files.
    write_tree(
        &files,
        &dest_root,
        false,
        scope.needs_sudo(),
        &format!("{}-au", p.bundle_id),
    )?;
    crate::log_output(format!(
        "      {} factory presets -> {}",
        files.len(),
        dest_root.display()
    ));
    Ok(())
}

/// Emit LV2 preset TTLs into a staged / installed `.lv2` bundle:
/// one `presets/<stem>.ttl` per preset plus the `manifest.ttl`
/// entries referencing them. Runs against the writable bundle (the
/// staging dir on macOS system scope), so no sudo handling here.
pub(crate) fn emit_lv2_presets(fp: &FactoryPresets, bundle: &Path, plugin_uri: &str) -> Res {
    let presets_dir = bundle.join("presets");
    fs_ctx::create_dir_all(&presets_dir)?;

    let mut manifest_additions = String::from(truce_build::lv2::PRESET_MANIFEST_PREFIXES);
    for pr in &fp.presets {
        let file_name = format!("{}.ttl", safe_filename(&pr.stem));
        let label = if pr.meta.category.is_empty() {
            pr.meta.name.clone()
        } else {
            // LV2 hosts show a flat label list; keep the category
            // visible the way subdirectories do it elsewhere.
            format!("{}/{}", pr.meta.category, pr.meta.name)
        };
        let ttl = truce_build::lv2::render_preset_ttl(plugin_uri, &pr.meta.uuid, &label, &pr.blob);
        fs_ctx::write(presets_dir.join(&file_name), &ttl)?;
        manifest_additions.push_str(&truce_build::lv2::render_preset_manifest_entry(
            plugin_uri,
            &pr.meta.uuid,
            &format!("presets/{file_name}"),
        ));
    }

    let manifest_path = bundle.join("manifest.ttl");
    let mut manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;
    manifest.push_str(&manifest_additions);
    fs_ctx::write(&manifest_path, &manifest)?;
    crate::log_output(format!(
        "      {} factory presets -> {}",
        fp.presets.len(),
        presets_dir.display()
    ));
    Ok(())
}
