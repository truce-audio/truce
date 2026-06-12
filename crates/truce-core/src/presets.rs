//! Runtime preset discovery: scope roots, the filesystem walk, and
//! preset-file loading.
//!
//! A preset on disk is a `.trucepreset` container
//! ([`truce_utils::preset`]) holding display metadata plus the
//! canonical state envelope. Format wrappers use this module to
//! surface presets to hosts: the CLAP wrapper declares the scope
//! roots to the host's preset indexer and parses files on demand;
//! property-driven formats (AU) enumerate eagerly via
//! [`enumerate_scope`].
//!
//! Factory presets live inside the installed plugin bundle (the
//! wrapper derives that root from its own on-disk location, which is
//! format- and OS-specific). User and pack presets live under the
//! per-OS root returned by [`user_preset_root`], shared by every
//! format so one library serves them all.

use std::path::{Path, PathBuf};

use crate::state::DeserializedState;
use truce_utils::preset::parse_preset_meta;
use truce_utils::safe_filename;

pub use truce_utils::preset::PRESET_FILE_EXT;

/// Where a preset lives. `Factory` presets sit inside the plugin
/// bundle, written at install time; `User` presets live in the
/// per-OS user directory; `Pack` presets came from a third-party
/// drop-in under the user root's `packs/` subdirectory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetScope {
    Factory,
    User,
    Pack,
}

/// One discovered preset: display metadata plus where to load it
/// from. The state blob is *not* held here - hosts enumerate far
/// more presets than they load, so the file is re-read lazily at
/// load time via [`load_preset_file`].
#[derive(Debug, Clone)]
pub struct PresetRef {
    /// Stable identity from the preset's metadata. Survives file
    /// rename, move, and recategorise; empty for files authored
    /// without one.
    pub uuid: String,
    /// `truce-preset://<vendor>/<plugin>/<uuid>` - see [`preset_uri`].
    pub uri: String,
    /// Human-readable name (from metadata, not the filename).
    pub name: String,
    /// Explicit metadata category, falling back to the preset's
    /// parent directory name within its scope root. `None` when
    /// neither exists.
    pub category: Option<String>,
    pub author: Option<String>,
    pub comment: Option<String>,
    pub tags: Vec<String>,
    /// The library's "init sound" marker from the metadata.
    pub default: bool,
    pub scope: PresetScope,
    /// Absolute path to the on-disk file.
    pub path: PathBuf,
}

/// The per-OS user-scope preset root for a plugin. One directory
/// serves every plugin format; third-party packs drop into a
/// `packs/<pack-name>/` subdirectory of it.
///
/// - macOS: `~/Library/Audio/Presets/truce/<vendor>/<plugin>/`
/// - Windows: `%APPDATA%\truce\<vendor>\<plugin>\presets\`
/// - Linux: `$XDG_DATA_HOME/truce/<vendor>/<plugin>/presets/`
///   (`~/.local/share/...` when `XDG_DATA_HOME` is unset)
///
/// Returns `None` when the relevant home / app-data environment
/// variable is missing (sandboxed or otherwise degenerate hosts);
/// callers skip the user scope in that case.
#[must_use]
pub fn user_preset_root(vendor: &str, plugin_name: &str) -> Option<PathBuf> {
    let vendor = safe_filename(vendor);
    let plugin = safe_filename(plugin_name);

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/Audio/Presets/truce")
                .join(vendor)
                .join(plugin),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(
            PathBuf::from(appdata)
                .join("truce")
                .join(vendor)
                .join(plugin)
                .join("presets"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Some(
            data_home
                .join("truce")
                .join(vendor)
                .join(plugin)
                .join("presets"),
        )
    }
}

/// Build the stable preset URI:
/// `truce-preset://<vendor>/<plugin>/<uuid>`.
///
/// Built from the metadata UUID (not the file path) so rename /
/// move / recategorise never breaks a host-side reference. Vendor
/// and plugin segments are sanitized the same way the on-disk
/// preset directories are, keeping URI and path derivation in sync.
#[must_use]
pub fn preset_uri(vendor: &str, plugin_name: &str, uuid: &str) -> String {
    format!(
        "truce-preset://{}/{}/{uuid}",
        safe_filename(vendor),
        safe_filename(plugin_name)
    )
}

/// Recursively walk one scope root for `.trucepreset` files and
/// parse each file's metadata block.
///
/// Files that fail to read or parse are skipped - a corrupt preset
/// shouldn't take down the host's scan, and the load path re-reports
/// failures for anything a user actually selects. A missing root
/// yields an empty list (the user scope doesn't exist until
/// something writes to it).
#[must_use]
pub fn enumerate_scope(
    root: &Path,
    scope: PresetScope,
    vendor: &str,
    plugin_name: &str,
) -> Vec<PresetRef> {
    let mut out = Vec::new();
    walk(root, root, scope, vendor, plugin_name, &mut out, 0);
    out
}

/// Directory-recursion ceiling for the preset walk. Preset libraries
/// are at most `packs/<pack>/<category>/` deep; anything deeper is a
/// mis-drop or a filesystem cycle via symlinks, and bailing beats
/// hanging the host's scan.
const MAX_WALK_DEPTH: usize = 6;

fn walk(
    root: &Path,
    dir: &Path,
    scope: PresetScope,
    vendor: &str,
    plugin_name: &str,
    out: &mut Vec<PresetRef>,
    depth: usize,
) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    // Deterministic enumeration order regardless of filesystem.
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, scope, vendor, plugin_name, out, depth + 1);
        } else if path.extension().and_then(|e| e.to_str()) == Some(PRESET_FILE_EXT)
            && let Some(preset) = read_preset_ref(Some(root), &path, scope, vendor, plugin_name)
        {
            out.push(preset);
        }
    }
}

/// Parse one preset file's metadata into a [`PresetRef`]. `root` is
/// the scope root the directory-derived category is computed against;
/// pass `None` (or the file's own parent) for a standalone file query
/// to suppress the fallback.
#[must_use]
pub fn read_preset_ref(
    root: Option<&Path>,
    path: &Path,
    scope: PresetScope,
    vendor: &str,
    plugin_name: &str,
) -> Option<PresetRef> {
    let bytes = std::fs::read(path).ok()?;
    let meta = parse_preset_meta(&bytes)?;

    // Explicit metadata category wins; otherwise the parent directory
    // name within the scope root (a file at the root itself has no
    // directory-derived category).
    let category = if meta.category.is_empty() {
        path.parent()
            .filter(|parent| Some(*parent) != root)
            .and_then(|parent| parent.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
    } else {
        Some(meta.category)
    };

    let none_if_empty = |s: String| if s.is_empty() { None } else { Some(s) };
    Some(PresetRef {
        uri: preset_uri(vendor, plugin_name, &meta.uuid),
        uuid: meta.uuid,
        name: meta.name,
        category,
        author: none_if_empty(meta.author),
        comment: none_if_empty(meta.comment),
        tags: meta.tags,
        default: meta.default,
        scope,
        path: path.to_path_buf(),
    })
}

/// Read a preset file and extract its state, validating the embedded
/// envelope against this plugin's identity hash. Returns `None` for
/// unreadable / malformed files and for presets saved by a different
/// plugin (a pack dropped into the wrong directory).
#[must_use]
pub fn load_preset_file(path: &Path, plugin_id_hash: u64) -> Option<DeserializedState> {
    let bytes = std::fs::read(path).ok()?;
    let (_, blob) = truce_utils::preset::parse_preset_file(&bytes)?;
    crate::state::deserialize_state(&blob, plugin_id_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use truce_utils::preset::{PresetMeta, write_preset_file};

    fn write_sample(dir: &Path, rel: &str, meta: &PresetMeta, blob: &[u8]) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, write_preset_file(meta, blob)).unwrap();
    }

    #[test]
    fn enumerates_with_directory_category_fallback() {
        let tmp = std::env::temp_dir().join("truce-presets-test-enum");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let explicit = PresetMeta {
            uuid: "u1".into(),
            name: "A".into(),
            category: "Lead".into(),
            ..PresetMeta::default()
        };
        let derived = PresetMeta {
            uuid: "u2".into(),
            name: "B".into(),
            ..PresetMeta::default()
        };
        let rootless = PresetMeta {
            uuid: "u3".into(),
            name: "C".into(),
            ..PresetMeta::default()
        };
        write_sample(&tmp, "pad/a.trucepreset", &explicit, &[]);
        write_sample(&tmp, "pad/b.trucepreset", &derived, &[]);
        write_sample(&tmp, "c.trucepreset", &rootless, &[]);
        // Non-preset files are ignored.
        std::fs::write(tmp.join("pad/readme.txt"), "x").unwrap();

        let refs = enumerate_scope(&tmp, PresetScope::Factory, "Acme", "Synth");
        assert_eq!(refs.len(), 3);
        let by_uuid = |u: &str| refs.iter().find(|r| r.uuid == u).unwrap();
        assert_eq!(by_uuid("u1").category.as_deref(), Some("Lead"));
        assert_eq!(by_uuid("u2").category.as_deref(), Some("pad"));
        assert_eq!(by_uuid("u3").category, None);
        assert_eq!(by_uuid("u1").uri, "truce-preset://Acme/Synth/u1");
        assert_eq!(by_uuid("u1").scope, PresetScope::Factory);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_root_is_empty() {
        let refs = enumerate_scope(
            Path::new("/nonexistent/truce-presets"),
            PresetScope::User,
            "V",
            "P",
        );
        assert!(refs.is_empty());
    }

    #[test]
    fn load_validates_plugin_hash() {
        let tmp = std::env::temp_dir().join("truce-presets-test-load");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let hash = crate::state::hash_plugin_id("com.acme.synth");
        let blob = crate::state::serialize_state(hash, &[0, 1], &[0.25, 8200.0], b"xs");
        let meta = PresetMeta {
            uuid: "u9".into(),
            name: "Loadable".into(),
            ..PresetMeta::default()
        };
        let path = tmp.join("loadable.trucepreset");
        std::fs::write(&path, write_preset_file(&meta, &blob)).unwrap();

        let state = load_preset_file(&path, hash).unwrap();
        assert_eq!(state.params, vec![(0, 0.25), (1, 8200.0)]);
        assert_eq!(state.extra.as_deref(), Some(&b"xs"[..]));
        assert!(load_preset_file(&path, hash ^ 1).is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
