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

use base64::Engine as _;

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
    let configured_dir = p.presets.as_ref().map(|c| c.factory_dir.clone());

    let Some(manifest) = crate::util::locate_plugin_manifest(root, &p.crate_name) else {
        if configured_dir.is_some() {
            return Err(format!(
                "[plugin.presets] set for \"{}\" but its crate dir could not be located",
                p.name
            )
            .into());
        }
        return Ok(None);
    };
    let crate_dir = manifest.parent().unwrap_or(root);
    let dir = crate_dir.join(configured_dir.as_deref().unwrap_or("presets"));

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

    let authored = truce_build::presets::read_presets_dir(&dir, true)?;
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
fn resolved_name<'a>(name_override: Option<&'a str>, name: &'a str) -> &'a str {
    match name_override {
        Some(n) if !n.is_empty() => n,
        _ => name,
    }
}

/// Serialize one `.vstpreset`: the Steinberg container with a single
/// `Comp` chunk holding the canonical state envelope (the same bytes
/// `truce-vst3`'s component `setState` consumes).
///
/// Layout: `"VST3"` magic, `i32` version, 32 ASCII hex chars of the
/// class ID, `i64` offset to the chunk list, the chunk data, then a
/// `"List"` section of `(id, offset, size)` entries - all integers
/// little-endian, per the VST3 SDK's `PresetFile` implementation.
fn vstpreset_bytes(class_id: &[u8; 16], blob: &[u8]) -> Vec<u8> {
    use std::fmt::Write as _;

    const HEADER_LEN: usize = 48;
    let mut out = Vec::with_capacity(HEADER_LEN + blob.len() + 36);
    out.extend_from_slice(b"VST3");
    out.extend_from_slice(&1i32.to_le_bytes());
    let mut hex = String::with_capacity(32);
    for b in class_id {
        let _ = write!(hex, "{b:02X}");
    }
    out.extend_from_slice(hex.as_bytes());
    let list_offset = HEADER_LEN + blob.len();
    out.extend_from_slice(&(list_offset as u64).to_le_bytes());
    out.extend_from_slice(blob);
    out.extend_from_slice(b"List");
    out.extend_from_slice(&1i32.to_le_bytes());
    out.extend_from_slice(b"Comp");
    out.extend_from_slice(&(HEADER_LEN as u64).to_le_bytes());
    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    out
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

/// Pack a 4-char code into the integer representation `.aupreset`
/// plists carry (`'aufx'` → big-endian u32).
#[cfg(target_os = "macos")]
fn fourcc_int(code: &str) -> Result<u32, crate::CargoTruceError> {
    let bytes = code.as_bytes();
    let four: [u8; 4] = bytes
        .try_into()
        .map_err(|_| format!("four-char code \"{code}\" is not exactly 4 bytes"))?;
    Ok(u32::from_be_bytes(four))
}

/// Render one `.aupreset` XML plist. The standard identity keys let
/// hosts match the preset to the component; the state itself rides
/// the `truce_state` key - the slot `truce-au`'s `ClassInfo` property
/// handler reads (and writes) the canonical envelope through.
#[cfg(target_os = "macos")]
fn aupreset_xml(au_type: u32, subtype: u32, manufacturer: u32, name: &str, blob: &[u8]) -> String {
    // 0x0001_0000: matches both the AudioComponents version in the
    // installed Info.plist and the registration descriptor.
    const AU_VERSION: u32 = 0x0001_0000;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>manufacturer</key>
    <integer>{manufacturer}</integer>
    <key>name</key>
    <string>{}</string>
    <key>subtype</key>
    <integer>{subtype}</integer>
    <key>truce_state</key>
    <data>{}</data>
    <key>type</key>
    <integer>{au_type}</integer>
    <key>version</key>
    <integer>{AU_VERSION}</integer>
</dict>
</plist>
"#,
        xml_escape(name),
        base64::engine::general_purpose::STANDARD.encode(blob),
    )
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vstpreset_layout_is_parseable() {
        let cid = [0xABu8; 16];
        let blob = vec![7u8; 10];
        let bytes = vstpreset_bytes(&cid, &blob);

        assert_eq!(&bytes[0..4], b"VST3");
        assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(&bytes[8..40], "AB".repeat(16).as_bytes());
        let list_offset =
            usize::try_from(u64::from_le_bytes(bytes[40..48].try_into().unwrap())).unwrap();
        assert_eq!(&bytes[48..58], &blob[..]);
        assert_eq!(&bytes[list_offset..list_offset + 4], b"List");
        let count = i32::from_le_bytes(bytes[list_offset + 4..list_offset + 8].try_into().unwrap());
        assert_eq!(count, 1);
        assert_eq!(&bytes[list_offset + 8..list_offset + 12], b"Comp");
        let comp_offset = u64::from_le_bytes(
            bytes[list_offset + 12..list_offset + 20]
                .try_into()
                .unwrap(),
        );
        let comp_size = u64::from_le_bytes(
            bytes[list_offset + 20..list_offset + 28]
                .try_into()
                .unwrap(),
        );
        assert_eq!(comp_offset, 48);
        assert_eq!(comp_size, blob.len() as u64);
        assert_eq!(bytes.len(), list_offset + 28);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn aupreset_contains_identity_and_state() {
        let xml = aupreset_xml(
            fourcc_int("aufx").unwrap(),
            fourcc_int("TGan").unwrap(),
            fourcc_int("Trce").unwrap(),
            "Bright & Saw",
            b"BLOB",
        );
        assert!(xml.contains("<integer>1635083896</integer>")); // 'aufx'
        assert!(xml.contains("Bright &amp; Saw"));
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"BLOB");
        assert!(xml.contains(&format!("<data>{encoded}</data>")));
        assert!(xml.contains("<key>truce_state</key>"));
    }
}
