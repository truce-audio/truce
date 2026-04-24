//! Preset authoring format + per-format fan-out.
//!
//! A plugin ships factory presets as `.preset` TOML files in a
//! `presets/` subdirectory. At install time the pipeline parses these
//! and emits per-format preset files (CLAP `.clap-preset`, VST3
//! `.vstpreset`, AU `.aupreset`, LV2 TTL, ...) alongside each format's
//! bundle. The plugin's format wrappers (`truce-clap`, etc.) read the
//! emitted per-format files at host-scan time and route them through
//! the existing `load_state` path.
//!
//! Design doc: `truce-docs/docs/internal/presets.md`.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub mod clap_preset;

/// Parsed `.preset` authoring file + filesystem-derived identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    /// Display name shown in the host's preset menu.
    pub name: String,
    /// Category bucket. Defaults to the parent directory name when
    /// absent from the file.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Exactly one preset per library may set this. See
    /// `read_presets_dir` for the enforcement.
    #[serde(default)]
    pub default: bool,
    /// `param-id -> value` map (both stored as strings in the TOML so
    /// id keys serialize cleanly; parsed on demand).
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, f64>,
    /// Optional opaque bytes that match the `extra` blob passed through
    /// `serialize_state`. Authors embed this as base64 in the
    /// `[extra]` table; `extra_bytes()` decodes.
    #[serde(default)]
    pub extra: Option<ExtraBytes>,

    /// Relative path from the library root, minus the `.preset`
    /// extension. Populated by `read_presets_dir`; empty when a preset
    /// is constructed in-memory. Used to derive the URI +
    /// the emitted per-format filename.
    #[serde(skip)]
    pub path_stem: String,
}

/// Base64-encoded opaque bytes embedded via `[extra] base64 = "..."`.
///
/// Split into its own struct so the TOML is readable:
/// ```toml
/// [extra]
/// base64 = "AAECAwQFBgc="
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraBytes {
    pub base64: String,
}

impl Preset {
    /// Parse a `.preset` TOML file into a `Preset`. The caller is
    /// responsible for setting `path_stem` + resolving `category`
    /// from the parent directory — `read_presets_dir` does both.
    pub fn from_toml(src: &str) -> Result<Self, PresetError> {
        toml::from_str(src).map_err(PresetError::Toml)
    }

    /// Decode the `[extra].base64` field into raw bytes.
    pub fn extra_bytes(&self) -> Result<Option<Vec<u8>>, PresetError> {
        match &self.extra {
            Some(e) => base64::engine::general_purpose::STANDARD
                .decode(e.base64.trim())
                .map(Some)
                .map_err(PresetError::Base64),
            None => Ok(None),
        }
    }

    /// Canonicalize the preset into the truce state blob the plugin
    /// consumes at load time. Parameter ids that don't parse as u32
    /// are silently skipped — same tolerance `deserialize_state`
    /// has for unknown ids. Caller supplies the hashed plugin id.
    pub fn to_state_blob(&self, plugin_id_hash: u64) -> Result<Vec<u8>, PresetError> {
        let mut ids = Vec::with_capacity(self.params.len());
        let mut values = Vec::with_capacity(self.params.len());
        for (k, v) in &self.params {
            let id: u32 = k.parse().map_err(|_| PresetError::BadParamId(k.clone()))?;
            ids.push(id);
            values.push(*v);
        }
        let extra = self.extra_bytes()?;
        Ok(truce_core::state::serialize_state(
            plugin_id_hash,
            &ids,
            &values,
            extra.as_deref(),
        ))
    }

    /// Effective category — the explicit `category` field, or the
    /// first path segment of `path_stem` (i.e. the parent dir name).
    pub fn effective_category(&self) -> &str {
        if let Some(c) = self.category.as_deref() {
            return c;
        }
        match self.path_stem.find('/') {
            Some(idx) => &self.path_stem[..idx],
            None => "",
        }
    }

    /// Base filename (no extension) the per-format emitter should use.
    pub fn stem(&self) -> &str {
        match self.path_stem.rfind('/') {
            Some(idx) => &self.path_stem[idx + 1..],
            None => &self.path_stem,
        }
    }
}

/// Walk a `presets/` directory and return every parsed `.preset` file.
///
/// Subdirectories become the default category. Files outside any
/// subdirectory get an empty category. Hidden files, non-`.preset`
/// extensions, and non-UTF-8 paths are silently skipped.
///
/// Returns an error if:
/// - a file fails to parse
/// - more than one preset sets `default = true`
pub fn read_presets_dir(root: &Path) -> Result<Vec<Preset>, PresetError> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by(|a, b| a.path_stem.cmp(&b.path_stem));

    let defaults: Vec<&str> = out
        .iter()
        .filter(|p| p.default)
        .map(|p| p.path_stem.as_str())
        .collect();
    if defaults.len() > 1 {
        return Err(PresetError::MultipleDefaults(
            defaults.iter().map(|s| s.to_string()).collect(),
        ));
    }
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Preset>) -> Result<(), PresetError> {
    let entries = fs::read_dir(dir).map_err(|e| PresetError::Io(dir.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| PresetError::Io(dir.to_path_buf(), e))?;
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        let ft = entry
            .file_type()
            .map_err(|e| PresetError::Io(path.clone(), e))?;
        if ft.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if !name.ends_with(".preset") {
            continue;
        }
        let src = fs::read_to_string(&path).map_err(|e| PresetError::Io(path.clone(), e))?;
        let mut preset = Preset::from_toml(&src).map_err(|e| match e {
            PresetError::Toml(err) => PresetError::Parse(path.clone(), err),
            other => other,
        })?;

        // `path_stem` is the relative path from the library root minus
        // the `.preset` extension, using forward slashes on every OS
        // so the derived URI stays stable cross-platform.
        let rel = path
            .strip_prefix(root)
            .map_err(|_| PresetError::BadPathPrefix(path.clone()))?;
        let rel_str = rel
            .to_str()
            .ok_or_else(|| PresetError::NonUtf8Path(path.clone()))?;
        let stem = rel_str
            .trim_end_matches(".preset")
            .replace(std::path::MAIN_SEPARATOR, "/");
        preset.path_stem = stem;

        // Derive default category from the parent directory *only* when
        // the file didn't set it explicitly. Explicit wins.
        if preset.category.is_none() {
            if let Some(parent) = rel.parent() {
                if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
                    if !name.is_empty() {
                        preset.category = Some(name.to_string());
                    }
                }
            }
        }

        out.push(preset);
    }
    Ok(())
}

/// Errors surfaced by the parser.
#[derive(Debug)]
pub enum PresetError {
    Io(PathBuf, std::io::Error),
    Toml(toml::de::Error),
    Parse(PathBuf, toml::de::Error),
    Base64(base64::DecodeError),
    BadParamId(String),
    MultipleDefaults(Vec<String>),
    BadPathPrefix(PathBuf),
    NonUtf8Path(PathBuf),
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresetError::Io(p, e) => write!(f, "{}: {}", p.display(), e),
            PresetError::Toml(e) => write!(f, "TOML: {e}"),
            PresetError::Parse(p, e) => write!(f, "{}: {}", p.display(), e),
            PresetError::Base64(e) => write!(f, "[extra].base64: {e}"),
            PresetError::BadParamId(k) => {
                write!(f, "preset [params] key '{k}' is not a u32 parameter id")
            }
            PresetError::MultipleDefaults(paths) => {
                write!(
                    f,
                    "multiple presets set `default = true`: {}",
                    paths.join(", ")
                )
            }
            PresetError::BadPathPrefix(p) => write!(f, "path outside library root: {}", p.display()),
            PresetError::NonUtf8Path(p) => write!(f, "non-UTF-8 path: {}", p.display()),
        }
    }
}

impl std::error::Error for PresetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let src = r#"
name = "Test"

[params]
0 = 0.5
1 = -12.0
"#;
        let p = Preset::from_toml(src).unwrap();
        assert_eq!(p.name, "Test");
        assert_eq!(p.params.get("0"), Some(&0.5));
        assert_eq!(p.params.get("1"), Some(&-12.0));
        assert!(!p.default);
    }

    #[test]
    fn parse_full() {
        let src = r#"
name = "Bright Saw"
category = "Lead"
author = "JK"
comment = "Classic"
tags = ["analog", "lead"]
default = true

[params]
0 = 0.75

[extra]
base64 = "AAECAw=="
"#;
        let p = Preset::from_toml(src).unwrap();
        assert_eq!(p.name, "Bright Saw");
        assert_eq!(p.category.as_deref(), Some("Lead"));
        assert_eq!(p.tags, vec!["analog", "lead"]);
        assert!(p.default);
        assert_eq!(p.extra_bytes().unwrap(), Some(vec![0, 1, 2, 3]));
    }

    #[test]
    fn to_state_blob_roundtrip() {
        let src = r#"
name = "T"

[params]
0 = 1.0
1 = 2.0
"#;
        let mut p = Preset::from_toml(src).unwrap();
        p.path_stem = "rooms/small".into();
        let hash = truce_core::state::hash_plugin_id("test.plugin");
        let blob = p.to_state_blob(hash).unwrap();
        let de = truce_core::state::deserialize_state(&blob, hash).unwrap();
        assert_eq!(de.params.len(), 2);
        assert!(de.params.contains(&(0, 1.0)));
        assert!(de.params.contains(&(1, 2.0)));
    }

    #[test]
    fn effective_category_prefers_explicit() {
        let mut p = Preset {
            name: "T".into(),
            category: Some("Halls".into()),
            author: None,
            comment: None,
            tags: vec![],
            default: false,
            params: Default::default(),
            extra: None,
            path_stem: "rooms/small".into(),
        };
        assert_eq!(p.effective_category(), "Halls");
        p.category = None;
        assert_eq!(p.effective_category(), "rooms");
    }

    #[test]
    fn read_dir_rejects_multiple_defaults() {
        let tmp = std::env::temp_dir().join(format!("truce-presets-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.preset"), "name=\"A\"\ndefault=true\n[params]\n0=0\n").unwrap();
        fs::write(tmp.join("b.preset"), "name=\"B\"\ndefault=true\n[params]\n0=0\n").unwrap();
        let err = read_presets_dir(&tmp).unwrap_err();
        assert!(matches!(err, PresetError::MultipleDefaults(_)));
        let _ = fs::remove_dir_all(&tmp);
    }
}
