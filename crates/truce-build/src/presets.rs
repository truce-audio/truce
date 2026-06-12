//! Authored-preset (`.preset` TOML) parsing and canonicalisation.
//!
//! The author-facing factory preset format: one human-readable TOML
//! file per preset, in a `presets/` directory next to the plugin
//! crate (first directory level = category). `cargo truce install`
//! reads them here, then re-envelopes each one into the per-format
//! native files (`.trucepreset`, `.vstpreset`, `.aupreset`, LV2 TTL).
//!
//! ```toml
//! name = "Bright Saw"
//! uuid = "9a2f6c1e-3b44-4f1d-9b7a-1de0c4a51b22"  # stamped on first install if missing
//! category = "Lead"     # optional; defaults to the parent directory name
//! author = "JK"         # optional
//! comment = "Classic bright sawtooth lead"       # optional
//! tags = ["analog", "lead"]                      # optional
//! default = false       # optional; at most one preset may set true
//! extra = "aGVsbG8="    # optional; base64 of the plugin's save_state() bytes
//!
//! [params]
//! # param-id = plain value (the same domain the param declares)
//! 0 = 0.75
//! 1 = 0.40
//! ```

use base64::Engine as _;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use truce_utils::preset::PresetMeta;

/// One parsed + canonicalised `.preset` file.
#[derive(Debug)]
pub struct AuthoredPreset {
    /// Display metadata, with `category` already resolved (explicit
    /// field, else parent directory name, else empty) and `default`
    /// carried through from the TOML.
    pub meta: PresetMeta,
    /// `(param id, plain value)` pairs in file order.
    pub params: Vec<(u32, f64)>,
    /// Decoded `extra` bytes (the plugin's `save_state()` payload).
    pub extra: Vec<u8>,
    /// Source file stem - the stable on-disk name per-format
    /// emitters reuse for their own files.
    pub stem: String,
    /// Source file path, for error reporting.
    pub path: PathBuf,
}

impl AuthoredPreset {
    /// Render this preset as the canonical state envelope for the
    /// plugin identified by `plugin_id_hash`
    /// ([`truce_utils::state::hash_plugin_id`] of the plugin ID).
    #[must_use]
    pub fn state_blob(&self, plugin_id_hash: u64) -> Vec<u8> {
        let ids: Vec<u32> = self.params.iter().map(|(id, _)| *id).collect();
        let values: Vec<f64> = self.params.iter().map(|(_, v)| *v).collect();
        truce_utils::state::serialize_state(plugin_id_hash, &ids, &values, &self.extra)
    }
}

#[derive(Deserialize)]
struct PresetFile {
    name: String,
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    author: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    extra: String,
    #[serde(default)]
    params: BTreeMap<String, toml::Value>,
}

/// Read every `.preset` file under `dir` (recursively, one directory
/// level is the conventional category layout but deeper nesting is
/// tolerated).
///
/// `stamp_missing_uuids` controls what happens to a preset authored
/// without a `uuid` field: `true` (the install pipeline) generates
/// one and prepends it to the source file, so the identity is minted
/// exactly once and survives subsequent installs; `false` makes a
/// missing uuid a hard error (read-only consumers).
///
/// # Errors
///
/// Returns a human-readable message for unreadable files, TOML
/// parse failures, malformed param keys / values, duplicate uuids,
/// or more than one `default = true`.
pub fn read_presets_dir(
    dir: &Path,
    stamp_missing_uuids: bool,
) -> Result<Vec<AuthoredPreset>, String> {
    let mut files = Vec::new();
    collect_preset_files(dir, &mut files, 0)
        .map_err(|e| format!("walking {}: {e}", dir.display()))?;
    files.sort();

    let mut presets = Vec::with_capacity(files.len());
    for path in files {
        presets.push(read_preset_file(dir, &path, stamp_missing_uuids)?);
    }

    // Library-level validation: identities and per-category display
    // names must be unique (host-facing preset files are named after
    // the display name), and at most one preset may claim the default
    // slot. Erroring beats silently picking one - the choice should
    // be visible in a diff.
    let mut seen_uuid: BTreeMap<&str, &Path> = BTreeMap::new();
    let mut seen_name: BTreeMap<(&str, &str), &Path> = BTreeMap::new();
    let mut default_path: Option<&Path> = None;
    for p in &presets {
        if let Some(first) = seen_uuid.insert(&p.meta.uuid, &p.path) {
            return Err(format!(
                "duplicate preset uuid \"{}\" in {} and {}",
                p.meta.uuid,
                first.display(),
                p.path.display()
            ));
        }
        if let Some(first) =
            seen_name.insert((p.meta.category.as_str(), p.meta.name.as_str()), &p.path)
        {
            return Err(format!(
                "duplicate preset name \"{}\" in category \"{}\": {} and {}",
                p.meta.name,
                p.meta.category,
                first.display(),
                p.path.display()
            ));
        }
        if p.meta.default {
            if let Some(first) = default_path {
                return Err(format!(
                    "multiple presets set `default = true`: {} and {}",
                    first.display(),
                    p.path.display()
                ));
            }
            default_path = Some(&p.path);
        }
    }

    Ok(presets)
}

fn collect_preset_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> std::io::Result<()> {
    // Mirrors the runtime walk's recursion ceiling: a preset library
    // deeper than this is a mis-drop or a symlink cycle.
    if depth > 6 || !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_preset_files(&path, out, depth + 1)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("preset") {
            out.push(path);
        }
    }
    Ok(())
}

fn read_preset_file(
    root: &Path,
    path: &Path,
    stamp_missing_uuids: bool,
) -> Result<AuthoredPreset, String> {
    let ctx = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let content = std::fs::read_to_string(path).map_err(|e| ctx(&e))?;
    let parsed: PresetFile = toml::from_str(&content).map_err(|e| ctx(&e))?;

    let uuid = if parsed.uuid.is_empty() {
        if !stamp_missing_uuids {
            return Err(format!(
                "{}: missing `uuid` field (run `cargo truce install` once to stamp one)",
                path.display()
            ));
        }
        let uuid = generate_uuid_v4();
        std::fs::write(path, format!("uuid = \"{uuid}\"\n{content}")).map_err(|e| ctx(&e))?;
        uuid
    } else {
        parsed.uuid
    };

    let mut params = Vec::with_capacity(parsed.params.len());
    for (key, value) in &parsed.params {
        let id: u32 = key.parse().map_err(|_| {
            format!(
                "{}: param key \"{key}\" is not a numeric param id",
                path.display()
            )
        })?;
        let plain = match value {
            toml::Value::Float(f) => *f,
            toml::Value::Integer(i) => {
                // Param values are f64 on the wire; the i64 → f64
                // precision loss above 2^53 is irrelevant for plain
                // parameter values.
                #[allow(clippy::cast_precision_loss)]
                {
                    *i as f64
                }
            }
            toml::Value::Boolean(b) => f64::from(u8::from(*b)),
            other => {
                return Err(format!(
                    "{}: param {key} has non-numeric value {other}",
                    path.display()
                ));
            }
        };
        params.push((id, plain));
    }

    let extra = if parsed.extra.is_empty() {
        Vec::new()
    } else {
        // Authors paste base64 from other tools, often line-wrapped;
        // strip whitespace before the strict decode.
        let compact: String = parsed.extra.split_whitespace().collect();
        base64::engine::general_purpose::STANDARD
            .decode(compact)
            .map_err(|e| format!("{}: `extra` is not valid base64: {e}", path.display()))?
    };

    // Explicit category wins; otherwise the parent directory name
    // within the library root (a file at the root has no category).
    let category = parsed.category.unwrap_or_default();
    let category = if category.is_empty() {
        path.parent()
            .filter(|parent| *parent != root)
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string()
    } else {
        category
    };

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    Ok(AuthoredPreset {
        meta: PresetMeta {
            uuid,
            name: parsed.name,
            category,
            author: parsed.author,
            comment: parsed.comment,
            tags: parsed.tags,
            default: parsed.default,
        },
        params,
        extra,
        stem,
        path: path.to_path_buf(),
    })
}

/// Generate a UUIDv4-shaped identifier from `std`'s process-seeded
/// `SipHash` entropy plus the wall clock. Uniqueness-grade (the only
/// property preset identity needs), not cryptographic.
fn generate_uuid_v4() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lib(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("truce-build-presets-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_and_canonicalises() {
        let dir = temp_lib("parse");
        std::fs::create_dir_all(dir.join("lead")).unwrap();
        std::fs::write(
            dir.join("lead/bright-saw.preset"),
            r#"
name = "Bright Saw"
uuid = "u-1"
author = "JK"
tags = ["analog", "lead"]
default = true
extra = "aGk="

[params]
0 = 0.75
2 = 1
5 = true
"#,
        )
        .unwrap();

        let presets = read_presets_dir(&dir, false).unwrap();
        assert_eq!(presets.len(), 1);
        let p = &presets[0];
        assert_eq!(p.meta.name, "Bright Saw");
        assert_eq!(p.meta.category, "lead");
        assert_eq!(p.params, vec![(0, 0.75), (2, 1.0), (5, 1.0)]);
        assert_eq!(p.extra, b"hi");
        assert!(p.meta.default);
        assert_eq!(p.stem, "bright-saw");

        let blob = p.state_blob(42);
        let state = truce_utils::state::deserialize_state(&blob, 42).unwrap();
        assert_eq!(state.params, p.params);
        assert_eq!(state.extra.as_deref(), Some(&b"hi"[..]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamps_missing_uuid_once() {
        let dir = temp_lib("stamp");
        let file = dir.join("init.preset");
        std::fs::write(&file, "name = \"Init\"\n").unwrap();

        assert!(read_presets_dir(&dir, false).is_err());

        let first = read_presets_dir(&dir, true).unwrap();
        let stamped = first[0].meta.uuid.clone();
        assert_eq!(stamped.len(), 36);
        assert_eq!(&stamped[14..15], "4");

        // Second read sees the same identity from the rewritten file.
        let second = read_presets_dir(&dir, false).unwrap();
        assert_eq!(second[0].meta.uuid, stamped);
        assert_eq!(second[0].meta.name, "Init");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_multiple_defaults_and_duplicate_uuids() {
        let dir = temp_lib("dups");
        std::fs::write(
            dir.join("a.preset"),
            "name = \"A\"\nuuid = \"u\"\ndefault = true\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.preset"),
            "name = \"B\"\nuuid = \"u\"\ndefault = true\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false).unwrap_err();
        assert!(err.contains("duplicate preset uuid"));

        std::fs::write(
            dir.join("b.preset"),
            "name = \"B\"\nuuid = \"v\"\ndefault = true\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false).unwrap_err();
        assert!(err.contains("default = true"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_params() {
        let dir = temp_lib("badparams");
        std::fs::write(
            dir.join("a.preset"),
            "name = \"A\"\nuuid = \"u\"\n[params]\ncutoff = 1.0\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false).unwrap_err();
        assert!(err.contains("not a numeric param id"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
