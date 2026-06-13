//! Preset library: scope roots, the filesystem walk, loading, and
//! the management (CRUD) API.
//!
//! A preset on disk is a `.trucepreset` container ([`crate::preset`])
//! holding display metadata plus the canonical state envelope.
//! Format wrappers use the discovery half to surface presets to
//! hosts (re-exported as `truce_core::presets`); in-editor preset
//! menus and `cargo truce preset` use [`PresetStore`] for the
//! management operations. Lives in `truce-utils` (std-only) so the
//! CLI shares one implementation with the runtime.
//!
//! Factory presets live inside the installed plugin bundle (the
//! wrapper derives that root from its own on-disk location, which is
//! format- and OS-specific). User and pack presets live under the
//! per-OS root returned by [`user_preset_root`], shared by every
//! format so one library serves them all.

use std::path::{Path, PathBuf};

use crate::preset::{PresetMeta, parse_preset_file, parse_preset_meta, write_preset_file};
use crate::safe_filename;
use crate::state::{DeserializedState, deserialize_state, serialize_state};

pub use crate::preset::PRESET_FILE_EXT;

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
/// `user_dir` is the optional `[plugin.presets]` `user_dir`
/// override from `truce.toml`. When `None` (or unusable - see
/// [`sanitize_preset_user_dir`]), the default subpath is
/// `truce/<vendor>/<plugin>`, with a trailing `presets` directory
/// on Windows / Linux. Where the path resolves per OS:
///
/// | OS | default | with `user_dir = "Acme/MySynth"` |
/// |---|---|---|
/// | macOS | `~/Library/Audio/Presets/truce/<vendor>/<plugin>/` | `~/Library/Audio/Presets/Acme/MySynth/` |
/// | Windows | `%APPDATA%\truce\<vendor>\<plugin>\presets\` | `%APPDATA%\Acme\MySynth\` |
/// | Linux | `$XDG_DATA_HOME/truce/<vendor>/<plugin>/presets/` | `$XDG_DATA_HOME/truce/Acme/MySynth/` |
///
/// (`$XDG_DATA_HOME` falls back to `~/.local/share` when unset.)
/// The override replaces the whole default subpath and no `presets`
/// suffix is appended - except on Linux, where it stays under the
/// `truce/` namespace: `$XDG_DATA_HOME` is a flat root shared by
/// every app, unlike macOS's preset-specific directory or the
/// per-vendor `%APPDATA%` convention.
///
/// Returns `None` when the relevant home / app-data environment
/// variable is missing (sandboxed or otherwise degenerate hosts);
/// callers skip the user scope in that case.
#[must_use]
pub fn user_preset_root(
    vendor: &str,
    plugin_name: &str,
    user_dir: Option<&str>,
) -> Option<PathBuf> {
    let override_subpath = user_dir.and_then(sanitize_preset_user_dir);
    let has_override = override_subpath.is_some();
    let subpath = override_subpath.unwrap_or_else(|| {
        PathBuf::from("truce")
            .join(safe_filename(vendor))
            .join(safe_filename(plugin_name))
    });

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        let _ = has_override;
        Some(
            PathBuf::from(home)
                .join("Library/Audio/Presets")
                .join(subpath),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")?;
        let mut root = PathBuf::from(appdata).join(subpath);
        if !has_override {
            root.push("presets");
        }
        Some(root)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        let mut root = data_home;
        if has_override {
            root.push("truce");
        }
        root.push(subpath);
        if !has_override {
            root.push("presets");
        }
        Some(root)
    }
}

/// Sanitize a `[plugin.presets]` `user_dir` override into a safe
/// relative subpath. The value is author-controlled text that lands
/// in filesystem paths, so it's interpreted strictly:
///
/// - split on `/` and `\`, each segment run through
///   [`safe_filename`] (drive colons, reserved characters, leading /
///   trailing dots all collapse);
/// - empty and `.` segments are dropped, which also neutralises
///   leading separators (absolute paths);
/// - any `..` segment rejects the whole override.
///
/// Returns `None` for an unusable value - resolvers fall back to
/// the default subpath, and `cargo truce` validates the field
/// loudly at install / preset time.
#[must_use]
pub fn sanitize_preset_user_dir(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for segment in raw.split(['/', '\\']) {
        let segment = segment.trim();
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return None;
        }
        let safe = safe_filename(segment);
        if !safe.is_empty() {
            out.push(safe);
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
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

/// Parse a [`preset_uri`]-shaped string into
/// `(vendor, plugin, uuid)`. Returns `None` for anything that isn't
/// a three-segment `truce-preset://` URI.
#[must_use]
pub fn parse_preset_uri(uri: &str) -> Option<(&str, &str, &str)> {
    let rest = uri.strip_prefix("truce-preset://")?;
    let mut segments = rest.splitn(3, '/');
    let vendor = segments.next().filter(|s| !s.is_empty())?;
    let plugin = segments.next().filter(|s| !s.is_empty())?;
    let uuid = segments.next().filter(|s| !s.is_empty())?;
    Some((vendor, plugin, uuid))
}

/// Generate a UUIDv4-shaped identifier from `std`'s process-seeded
/// `SipHash` entropy plus the wall clock. Uniqueness-grade (the only
/// property preset identity needs), not cryptographic.
#[must_use]
pub fn mint_uuid() -> String {
    use std::fmt::Write as _;
    use std::hash::{BuildHasher, Hasher};
    let mix = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::from(d.subsec_nanos()));
        h.finish() ^ nanos.rotate_left(17)
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&mix(0x9e37_79b9).to_le_bytes());
    bytes[8..].copy_from_slice(&mix(0x85eb_ca6b).to_le_bytes());
    // RFC 4122 version (4) and variant (10x) bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut out = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{b:02x}");
    }
    out
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
    let (_, blob) = parse_preset_file(&bytes)?;
    deserialize_state(&blob, plugin_id_hash)
}

// ---------------------------------------------------------------------------
// Management (CRUD)
// ---------------------------------------------------------------------------

/// Why a [`PresetStore`] operation failed.
#[derive(Debug)]
pub enum PresetError {
    /// No preset with that URI / uuid exists in any scope.
    NotFound,
    /// The operation mutates, but the preset lives in the factory
    /// or pack scope - both are read-only from the runtime's
    /// perspective (`cargo truce install` owns factory; pack files
    /// belong to their distributor).
    ReadOnlyScope,
    /// The per-OS user preset directory could not be resolved
    /// (missing home / app-data environment).
    NoUserDirectory,
    /// The preset's embedded state envelope doesn't parse or belongs
    /// to a different plugin.
    InvalidState,
    /// The display name is empty after sanitization.
    InvalidName,
    Io(std::io::Error),
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("preset not found"),
            Self::ReadOnlyScope => f.write_str("factory / pack presets are read-only"),
            Self::NoUserDirectory => f.write_str("user preset directory could not be resolved"),
            Self::InvalidState => f.write_str("preset state is malformed or from another plugin"),
            Self::InvalidName => f.write_str("preset name is empty"),
            Self::Io(e) => write!(f, "preset io: {e}"),
        }
    }
}

impl std::error::Error for PresetError {}

impl From<std::io::Error> for PresetError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// One plugin's preset library across all three scopes, with the
/// management operations layered on top of the discovery walk.
///
/// `enumerate` applies the identity rule: two presets with the same
/// uuid in different scopes are the same logical preset, and the
/// more user-proximate copy wins (User over Pack over Factory) so a
/// user "override" of a factory preset shows once, not twice.
/// Mutations only ever touch the user scope.
pub struct PresetStore {
    vendor: String,
    plugin_name: String,
    plugin_id_hash: u64,
    factory_root: Option<PathBuf>,
    user_root: Option<PathBuf>,
}

impl PresetStore {
    /// A store for the given plugin identity. The user root resolves
    /// from the per-OS environment, honouring the optional
    /// `[plugin.presets]` `user_dir` override (see
    /// [`user_preset_root`] for where the path lands per OS); the
    /// factory root (inside the installed bundle) is format-specific,
    /// so callers that have one add it via
    /// [`Self::with_factory_root`].
    #[must_use]
    pub fn new(
        vendor: &str,
        plugin_name: &str,
        plugin_id_hash: u64,
        user_dir: Option<&str>,
    ) -> Self {
        Self {
            vendor: vendor.to_string(),
            plugin_name: plugin_name.to_string(),
            plugin_id_hash,
            factory_root: None,
            user_root: user_preset_root(vendor, plugin_name, user_dir),
        }
    }

    #[must_use]
    pub fn with_factory_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.factory_root = Some(root.into());
        self
    }

    /// Override the user root (tests, tools operating on a staged
    /// library).
    #[must_use]
    pub fn with_user_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.user_root = Some(root.into());
        self
    }

    #[must_use]
    pub fn user_root(&self) -> Option<&Path> {
        self.user_root.as_deref()
    }

    /// Every preset across factory, user, and pack scopes, deduped
    /// by uuid (User wins over Pack wins over Factory), ordered
    /// factory-first then by category / name within each scope.
    ///
    /// User / pack files that arrived without a uuid (hand-assembled
    /// packs, pre-tool files) get one minted and written back on
    /// first read, so their identity is stable from then on;
    /// write-back failures degrade to an empty uuid rather than
    /// hiding the preset.
    #[must_use]
    pub fn enumerate(&self) -> Vec<PresetRef> {
        let mut user: Vec<PresetRef> = Vec::new();
        let mut packs: Vec<PresetRef> = Vec::new();
        if let Some(root) = &self.user_root {
            let packs_root = root.join("packs");
            for mut preset in
                enumerate_scope(root, PresetScope::User, &self.vendor, &self.plugin_name)
            {
                if preset.uuid.is_empty() {
                    self.stamp_uuid(&mut preset);
                }
                if preset.path.starts_with(&packs_root) {
                    preset.scope = PresetScope::Pack;
                    packs.push(preset);
                } else {
                    user.push(preset);
                }
            }
        }
        let factory = self.factory_root.as_ref().map_or_else(Vec::new, |root| {
            enumerate_scope(root, PresetScope::Factory, &self.vendor, &self.plugin_name)
        });

        // Precedence order for the dedup pass; presentation order is
        // re-sorted below.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<PresetRef> = Vec::new();
        for preset in user.into_iter().chain(packs).chain(factory) {
            if !preset.uuid.is_empty() && !seen.insert(preset.uuid.clone()) {
                continue;
            }
            out.push(preset);
        }
        out.sort_by(|a, b| {
            scope_rank(a.scope)
                .cmp(&scope_rank(b.scope))
                .then_with(|| a.category.cmp(&b.category))
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// Mint and persist a uuid for a user / pack preset that has
    /// none. Best-effort: on any failure the preset keeps its empty
    /// uuid for this enumeration.
    fn stamp_uuid(&self, preset: &mut PresetRef) {
        let Ok(bytes) = std::fs::read(&preset.path) else {
            return;
        };
        let Some((mut meta, blob)) = parse_preset_file(&bytes) else {
            return;
        };
        meta.uuid = mint_uuid();
        if std::fs::write(&preset.path, write_preset_file(&meta, &blob)).is_ok() {
            preset.uri = preset_uri(&self.vendor, &self.plugin_name, &meta.uuid);
            preset.uuid = meta.uuid;
        }
    }

    /// Resolve a preset by `truce-preset://` URI, bare uuid, or
    /// display name. A URI for a different vendor / plugin returns
    /// `None`. Name matching is a fallback after uuid (names are
    /// unique per category by library validation; on a cross-category
    /// name clash the first in enumeration order wins).
    #[must_use]
    pub fn find(&self, sel: &str) -> Option<PresetRef> {
        let uuid = if let Some((vendor, plugin, uuid)) = parse_preset_uri(sel) {
            if vendor != safe_filename(&self.vendor) || plugin != safe_filename(&self.plugin_name) {
                return None;
            }
            uuid
        } else {
            sel
        };
        if uuid.is_empty() {
            return None;
        }
        let presets = self.enumerate();
        presets
            .iter()
            .find(|p| p.uuid == uuid)
            .or_else(|| presets.iter().find(|p| p.name == sel))
            .cloned()
    }

    /// Load a preset's state, validating it against this plugin's
    /// identity hash.
    ///
    /// # Errors
    ///
    /// [`PresetError::NotFound`] for an unknown URI;
    /// [`PresetError::InvalidState`] when the file's envelope doesn't
    /// parse or belongs to a different plugin.
    pub fn load(&self, uri_or_uuid: &str) -> Result<DeserializedState, PresetError> {
        let preset = self.find(uri_or_uuid).ok_or(PresetError::NotFound)?;
        load_preset_file(&preset.path, self.plugin_id_hash).ok_or(PresetError::InvalidState)
    }

    /// Save a preset into the user scope.
    ///
    /// A user preset with the same `(category, name)` is overwritten
    /// in place, keeping its uuid - the natural "Save" gesture.
    /// Otherwise a new file is created with a minted uuid (or
    /// `meta.uuid` when the caller pre-assigned one). `meta.name` is
    /// the display name; the file lands at
    /// `<user root>/<category>/<name>.trucepreset`.
    ///
    /// # Errors
    ///
    /// [`PresetError::NoUserDirectory`] when the per-OS root can't be
    /// resolved, [`PresetError::InvalidName`] for a name that
    /// sanitizes to nothing, [`PresetError::Io`] on write failure.
    pub fn save(
        &self,
        mut meta: PresetMeta,
        params: &[(u32, f64)],
        extra: &[u8],
    ) -> Result<PresetRef, PresetError> {
        let user_root = self
            .user_root
            .as_ref()
            .ok_or(PresetError::NoUserDirectory)?;
        let file_stem = safe_filename(&meta.name);
        if file_stem.is_empty() {
            return Err(PresetError::InvalidName);
        }

        let existing = self.enumerate().into_iter().find(|p| {
            p.scope == PresetScope::User
                && p.name == meta.name
                && p.category.as_deref().unwrap_or_default()
                    == if meta.category.is_empty() {
                        ""
                    } else {
                        meta.category.as_str()
                    }
        });
        let path = if let Some(existing) = &existing {
            meta.uuid.clone_from(&existing.uuid);
            existing.path.clone()
        } else {
            if meta.uuid.is_empty() {
                meta.uuid = mint_uuid();
            }
            let dir = if meta.category.is_empty() {
                user_root.clone()
            } else {
                user_root.join(safe_filename(&meta.category))
            };
            dir.join(format!("{file_stem}.{PRESET_FILE_EXT}"))
        };

        let ids: Vec<u32> = params.iter().map(|(id, _)| *id).collect();
        let values: Vec<f64> = params.iter().map(|(_, v)| *v).collect();
        let blob = serialize_state(self.plugin_id_hash, &ids, &values, extra);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, write_preset_file(&meta, &blob))?;
        read_preset_ref(
            self.user_root.as_deref(),
            &path,
            PresetScope::User,
            &self.vendor,
            &self.plugin_name,
        )
        .ok_or(PresetError::InvalidState)
    }

    /// Rename a user preset's display name. The uuid (and therefore
    /// the URI and any host-side reference) is unchanged; the file
    /// keeps its on-disk name.
    ///
    /// # Errors
    ///
    /// [`PresetError::NotFound`], [`PresetError::ReadOnlyScope`] for
    /// factory / pack presets, [`PresetError::InvalidState`] for a
    /// file that no longer parses, [`PresetError::Io`].
    pub fn rename(&self, uri_or_uuid: &str, new_name: &str) -> Result<(), PresetError> {
        let preset = self.user_preset(uri_or_uuid)?;
        rewrite_meta(&preset.path, |meta| new_name.clone_into(&mut meta.name))
    }

    /// Move a user preset to a different category. Rewrites the
    /// explicit `category` metadata *and* moves the file into the
    /// matching directory so the on-disk layout stays readable. The
    /// uuid is unchanged.
    ///
    /// # Errors
    ///
    /// Same surface as [`Self::rename`], plus
    /// [`PresetError::NoUserDirectory`].
    pub fn recategorise(&self, uri_or_uuid: &str, new_category: &str) -> Result<(), PresetError> {
        let user_root = self
            .user_root
            .as_ref()
            .ok_or(PresetError::NoUserDirectory)?;
        let preset = self.user_preset(uri_or_uuid)?;

        rewrite_meta(&preset.path, |meta| {
            new_category.clone_into(&mut meta.category);
        })?;

        let dir = if new_category.is_empty() {
            user_root.clone()
        } else {
            user_root.join(safe_filename(new_category))
        };
        let file_name = preset
            .path
            .file_name()
            .ok_or(PresetError::InvalidState)?
            .to_os_string();
        let dest = dir.join(file_name);
        if dest != preset.path {
            std::fs::create_dir_all(&dir)?;
            std::fs::rename(&preset.path, &dest)?;
        }
        Ok(())
    }

    /// Delete a user preset.
    ///
    /// # Errors
    ///
    /// [`PresetError::NotFound`], [`PresetError::ReadOnlyScope`] for
    /// factory / pack presets, [`PresetError::Io`].
    pub fn delete(&self, uri_or_uuid: &str) -> Result<(), PresetError> {
        let preset = self.user_preset(uri_or_uuid)?;
        std::fs::remove_file(&preset.path)?;
        Ok(())
    }

    fn user_preset(&self, uri_or_uuid: &str) -> Result<PresetRef, PresetError> {
        let preset = self.find(uri_or_uuid).ok_or(PresetError::NotFound)?;
        if preset.scope != PresetScope::User {
            return Err(PresetError::ReadOnlyScope);
        }
        Ok(preset)
    }
}

fn rewrite_meta(path: &Path, edit: impl FnOnce(&mut PresetMeta)) -> Result<(), PresetError> {
    let bytes = std::fs::read(path)?;
    let (mut meta, blob) = parse_preset_file(&bytes).ok_or(PresetError::InvalidState)?;
    edit(&mut meta);
    std::fs::write(path, write_preset_file(&meta, &blob))?;
    Ok(())
}

fn scope_rank(scope: PresetScope) -> u8 {
    match scope {
        PresetScope::Factory => 0,
        PresetScope::User => 1,
        PresetScope::Pack => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_sample(dir: &Path, rel: &str, meta: &PresetMeta, blob: &[u8]) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, write_preset_file(meta, blob)).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("truce-presets-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store(name: &str) -> (PresetStore, PathBuf, PathBuf) {
        let user = temp_dir(&format!("{name}-user"));
        let factory = temp_dir(&format!("{name}-factory"));
        let store = PresetStore::new("Acme", "Synth", 42, None)
            .with_user_root(&user)
            .with_factory_root(&factory);
        (store, user, factory)
    }

    fn meta(uuid: &str, name: &str, category: &str) -> PresetMeta {
        PresetMeta {
            uuid: uuid.into(),
            name: name.into(),
            category: category.into(),
            ..PresetMeta::default()
        }
    }

    #[test]
    fn enumerates_with_directory_category_fallback() {
        let tmp = temp_dir("enum");
        write_sample(&tmp, "pad/a.trucepreset", &meta("u1", "A", "Lead"), &[]);
        write_sample(&tmp, "pad/b.trucepreset", &meta("u2", "B", ""), &[]);
        write_sample(&tmp, "c.trucepreset", &meta("u3", "C", ""), &[]);
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
        let tmp = temp_dir("load");
        let hash = crate::state::hash_plugin_id("com.acme.synth");
        let blob = serialize_state(hash, &[0, 1], &[0.25, 8200.0], b"xs");
        let path = tmp.join("loadable.trucepreset");
        std::fs::write(&path, write_preset_file(&meta("u9", "Loadable", ""), &blob)).unwrap();

        let state = load_preset_file(&path, hash).unwrap();
        assert_eq!(state.params, vec![(0, 0.25), (1, 8200.0)]);
        assert_eq!(state.extra.as_deref(), Some(&b"xs"[..]));
        assert!(load_preset_file(&path, hash ^ 1).is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sanitize_user_dir_rules() {
        let ok = |raw: &str, want: &str| {
            assert_eq!(
                sanitize_preset_user_dir(raw),
                Some(PathBuf::from(want)),
                "{raw}"
            );
        };
        ok("Acme/MySynth", "Acme/MySynth");
        ok("Acme\\MySynth", "Acme/MySynth"); // windows separators
        ok("/Acme/MySynth/", "Acme/MySynth"); // absolute neutralised
        ok("Acme/./MySynth", "Acme/MySynth"); // `.` dropped
        ok("C:/Acme", "C/Acme"); // drive colon collapses
        ok(" Acme / My Synth ", "Acme/My Synth"); // trimmed, spaces kept

        assert_eq!(sanitize_preset_user_dir("../escape"), None);
        assert_eq!(sanitize_preset_user_dir("a/../b"), None);
        assert_eq!(sanitize_preset_user_dir(""), None);
        assert_eq!(sanitize_preset_user_dir("///"), None);
        // `safe_filename` trims dot runs, leaving no usable segment.
        assert_eq!(sanitize_preset_user_dir("..."), None);
    }

    #[test]
    fn user_root_honours_override() {
        let default = user_preset_root("Acme", "My Synth", None).unwrap();
        let overridden = user_preset_root("Acme", "My Synth", Some("AcmeAudio/Synth")).unwrap();
        let unusable = user_preset_root("Acme", "My Synth", Some("../nope")).unwrap();

        assert!(
            default.ends_with("truce/Acme/My Synth")
                || default.ends_with("truce/Acme/My Synth/presets")
        );
        assert!(overridden.to_string_lossy().contains("AcmeAudio"));
        assert!(overridden.ends_with("AcmeAudio/Synth"));
        // Unusable override falls back to the default path.
        assert_eq!(unusable, default);
    }

    #[test]
    fn uri_round_trips() {
        let uri = preset_uri("Acme Co", "My Synth", "u-1");
        assert_eq!(parse_preset_uri(&uri), Some(("Acme Co", "My Synth", "u-1")));
        assert_eq!(parse_preset_uri("nope://x/y/z"), None);
        assert_eq!(parse_preset_uri("truce-preset://only/two"), None);
    }

    #[test]
    fn mint_uuid_is_v4_shaped_and_distinct() {
        let a = mint_uuid();
        let b = mint_uuid();
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert_ne!(a, b);
    }

    #[test]
    fn user_overrides_factory_and_packs_classify() {
        let (store, user, factory) = store("dedup");
        let blob = serialize_state(42, &[0], &[1.0], &[]);
        write_sample(&factory, "lead/a.trucepreset", &meta("u1", "A", ""), &blob);
        write_sample(&factory, "lead/b.trucepreset", &meta("u2", "B", ""), &blob);
        // User override of u1 + a pack drop-in.
        write_sample(
            &user,
            "my/a2.trucepreset",
            &meta("u1", "A edited", ""),
            &blob,
        );
        write_sample(
            &user,
            "packs/edm/lead/p.trucepreset",
            &meta("u4", "P", ""),
            &blob,
        );

        let refs = store.enumerate();
        assert_eq!(refs.len(), 3);
        let a = refs.iter().find(|r| r.uuid == "u1").unwrap();
        assert_eq!(a.scope, PresetScope::User);
        assert_eq!(a.name, "A edited");
        assert_eq!(
            refs.iter().find(|r| r.uuid == "u4").unwrap().scope,
            PresetScope::Pack
        );

        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&factory);
    }

    #[test]
    fn stamps_missing_uuid_on_user_files() {
        let (store, user, factory) = store("stamp");
        let blob = serialize_state(42, &[], &[], &[]);
        write_sample(&user, "x.trucepreset", &meta("", "Handmade", ""), &blob);

        let refs = store.enumerate();
        let stamped = &refs[0];
        assert_eq!(stamped.uuid.len(), 36);
        // Persisted: a second enumeration sees the same identity.
        let again = store.enumerate();
        assert_eq!(again[0].uuid, stamped.uuid);

        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&factory);
    }

    #[test]
    fn save_load_rename_recategorise_delete() {
        let (store, user, factory) = store("crud");

        let saved = store
            .save(meta("", "My Lead", "lead"), &[(0, 0.5), (1, 440.0)], b"x")
            .unwrap();
        assert_eq!(saved.scope, PresetScope::User);
        assert!(saved.path.starts_with(user.join("lead")));
        assert_eq!(saved.uuid.len(), 36);

        let state = store.load(&saved.uri).unwrap();
        assert_eq!(state.params, vec![(0, 0.5), (1, 440.0)]);

        // find resolves by uri, uuid, and display name.
        assert_eq!(store.find(&saved.uri).unwrap().uuid, saved.uuid);
        assert_eq!(store.find(&saved.uuid).unwrap().uuid, saved.uuid);
        assert_eq!(store.find("My Lead").unwrap().uuid, saved.uuid);
        assert!(store.find("nonexistent").is_none());

        // Same (category, name) saves in place, keeping the uuid.
        let resaved = store
            .save(meta("", "My Lead", "lead"), &[(0, 0.9)], &[])
            .unwrap();
        assert_eq!(resaved.uuid, saved.uuid);
        assert_eq!(store.enumerate().len(), 1);

        store.rename(&saved.uuid, "Better Lead").unwrap();
        let renamed = store.find(&saved.uuid).unwrap();
        assert_eq!(renamed.name, "Better Lead");
        assert_eq!(renamed.uri, saved.uri);

        store.recategorise(&saved.uuid, "bass").unwrap();
        let moved = store.find(&saved.uuid).unwrap();
        assert_eq!(moved.category.as_deref(), Some("bass"));
        assert!(moved.path.starts_with(user.join("bass")));

        store.delete(&saved.uuid).unwrap();
        assert!(store.find(&saved.uuid).is_none());
        assert!(matches!(
            store.delete(&saved.uuid),
            Err(PresetError::NotFound)
        ));

        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&factory);
    }

    #[test]
    fn factory_presets_are_read_only() {
        let (store, user, factory) = store("readonly");
        let blob = serialize_state(42, &[], &[], &[]);
        write_sample(&factory, "a.trucepreset", &meta("u1", "A", ""), &blob);

        assert!(matches!(
            store.rename("u1", "B"),
            Err(PresetError::ReadOnlyScope)
        ));
        assert!(matches!(
            store.delete("u1"),
            Err(PresetError::ReadOnlyScope)
        ));

        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&factory);
    }
}
