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
/// `[params]` keys are either numeric param ids or Rust field
/// identifiers (`cutoff = 8200.0`); named keys resolve through
/// `names`, built from the `derive(Params)` sidecars. Pass `None`
/// to accept numeric keys only (named keys then error with a
/// build-the-plugin hint).
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
    names: Option<&ParamNameMap>,
) -> Result<Vec<AuthoredPreset>, String> {
    let mut files = Vec::new();
    collect_preset_files(dir, &mut files, 0)
        .map_err(|e| format!("walking {}: {e}", dir.display()))?;
    files.sort();

    let mut presets = Vec::with_capacity(files.len());
    for path in files {
        presets.push(read_preset_file(dir, &path, stamp_missing_uuids, names)?);
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
    names: Option<&ParamNameMap>,
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
        let uuid = truce_utils::presets::mint_uuid();
        std::fs::write(path, format!("uuid = \"{uuid}\"\n{content}")).map_err(|e| ctx(&e))?;
        uuid
    } else {
        parsed.uuid
    };

    let mut params = Vec::with_capacity(parsed.params.len());
    for (key, value) in &parsed.params {
        // Numeric keys are ids and need no schema; anything else is
        // a Rust field identifier resolved through the sidecars.
        let id: u32 = if key.bytes().all(|b| b.is_ascii_digit()) && !key.is_empty() {
            key.parse()
                .map_err(|_| format!("{}: param id \"{key}\" out of range", path.display()))?
        } else {
            match names {
                Some(map) if !map.is_empty() => map
                    .resolve(key)
                    .map_err(|e| format!("{}: {e}", path.display()))?,
                _ => {
                    return Err(format!(
                        "{}: param \"{key}\" is a name, but no param manifest is \
                         available - build the plugin once (`cargo build -p <crate>`) \
                         so the derive sidecars exist, or use the numeric id",
                        path.display()
                    ));
                }
            }
        };
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
        if params.iter().any(|(existing, _)| *existing == id) {
            return Err(format!(
                "{}: param {id} is set twice (a name and an id can refer to \
                 the same param)",
                path.display()
            ));
        }
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

/// Parse a single `.preset` file outside a library walk (the
/// `cargo truce preset convert` input path). The category falls back
/// to the parent directory name like the library walk does.
///
/// # Errors
///
/// Same per-file surface as [`read_presets_dir`]; a missing uuid is
/// an error (single-file reads never stamp).
pub fn read_single_preset(
    path: &Path,
    names: Option<&ParamNameMap>,
) -> Result<AuthoredPreset, String> {
    let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
    // The parent dir doubles as the walk root so no directory-derived
    // category applies - explicit `category` still does.
    read_preset_file(&root, path, false, names)
}

/// Per-param info from the `derive(Params)` sidecars: the Rust
/// field identifier (the canonical `.preset` key), the display
/// name, and the unit (the latter two used for generated comments).
#[derive(Debug, Clone)]
pub struct ParamAnnotation {
    /// Rust field identifier (`cutoff`). Empty for sidecars written
    /// before the field line existed; rebuild the plugin to refresh.
    pub field: String,
    pub name: String,
    pub unit: String,
}

/// Read the param-name / unit table from the `derive(Params)`
/// sidecars at `<target>/lv2-meta/<crate>/*.params.toml`. Returns an
/// empty map when the plugin hasn't been built yet (annotations are
/// a nicety, not a requirement). Sidecars exist per params struct,
/// including helpers; ids are unique within a plugin, so a plain
/// union suffices.
#[must_use]
pub fn read_param_annotations(
    sidecar_dir: &Path,
) -> std::collections::BTreeMap<u32, ParamAnnotation> {
    let mut out = std::collections::BTreeMap::new();
    // `param_index.toml` is the flattened, base-resolved id->field table
    // the root `derive(Params)` writes. Prefer it: the per-struct
    // sidecars carry struct-local ids that collide across `#[nested]`
    // groups, so unioning them mis-resolves a nested plugin's keys.
    let index = sidecar_dir.join("param_index.toml");
    let sources: Vec<std::path::PathBuf> = if index.is_file() {
        vec![index]
    } else {
        let Ok(entries) = std::fs::read_dir(sidecar_dir) else {
            return out;
        };
        entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
            .collect()
    };
    for path in sources {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = content.parse::<toml::Table>() else {
            continue;
        };
        let Some(params) = doc.get("param").and_then(toml::Value::as_array) else {
            continue;
        };
        for p in params {
            let Some(id) = p
                .get("id")
                .and_then(toml::Value::as_integer)
                .and_then(|i| u32::try_from(i).ok())
            else {
                continue;
            };
            let field = |key: &str| {
                p.get(key)
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            out.insert(
                id,
                ParamAnnotation {
                    field: field("field"),
                    name: field("name"),
                    unit: field("unit"),
                },
            );
        }
    }
    out
}

/// Render the `id -> lv2:symbol` table (from
/// [`crate::lv2::resolved_param_symbols`]) as the `symbols.toml`
/// sidecar the derive macro writes next to `plugin.ttl`. The install
/// step reads it back with [`read_param_symbols`] to emit a preset's
/// `lv2:port` / `pset:value` entries with the exact symbols the
/// manifest declared (collision resolution needs the full param list,
/// which only exists at manifest-aggregation time).
#[must_use]
pub fn render_param_symbols(symbols: &[(u32, String)]) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    for (id, symbol) in symbols {
        let _ = writeln!(buf, "[[symbol]]");
        let _ = writeln!(buf, "id = {id}");
        let _ = writeln!(buf, "symbol = \"{}\"\n", toml_escape(symbol));
    }
    buf
}

/// Read the `id -> lv2:symbol` table written by [`render_param_symbols`]
/// from `<sidecar_dir>/symbols.toml`. Empty when the file is missing
/// (plugin built before this existed - rebuild to refresh) or
/// unparseable; the caller then falls back to a `state:state`-only
/// preset.
#[must_use]
pub fn read_param_symbols(sidecar_dir: &Path) -> std::collections::BTreeMap<u32, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(content) = std::fs::read_to_string(sidecar_dir.join("symbols.toml")) else {
        return out;
    };
    let Ok(doc) = content.parse::<toml::Table>() else {
        return out;
    };
    let Some(symbols) = doc.get("symbol").and_then(toml::Value::as_array) else {
        return out;
    };
    for s in symbols {
        let Some(id) = s
            .get("id")
            .and_then(toml::Value::as_integer)
            .and_then(|i| u32::try_from(i).ok())
        else {
            continue;
        };
        let Some(symbol) = s.get("symbol").and_then(toml::Value::as_str) else {
            continue;
        };
        out.insert(id, symbol.to_string());
    }
    out
}

/// Resolves `.preset` param keys written as Rust field identifiers
/// (`cutoff = 8200.0`) to param ids. Built from the same sidecar
/// annotations the comment generator uses; names are authoring
/// sugar only - the wire format stores ids.
#[derive(Debug, Default)]
pub struct ParamNameMap {
    entries: std::collections::BTreeMap<String, NameBinding>,
}

#[derive(Debug)]
enum NameBinding {
    Id(u32),
    /// The same field identifier appears in more than one nested
    /// params struct with different ids - unusable as a key.
    Ambiguous(Vec<u32>),
}

impl ParamNameMap {
    #[must_use]
    pub fn from_annotations(
        annotations: &std::collections::BTreeMap<u32, ParamAnnotation>,
    ) -> Self {
        let mut entries: std::collections::BTreeMap<String, NameBinding> =
            std::collections::BTreeMap::new();
        for (&id, a) in annotations {
            if a.field.is_empty() {
                continue;
            }
            match entries.entry(a.field.clone()) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(NameBinding::Id(id));
                }
                std::collections::btree_map::Entry::Occupied(mut e) => match e.get_mut() {
                    NameBinding::Id(first) if *first != id => {
                        let first = *first;
                        e.insert(NameBinding::Ambiguous(vec![first, id]));
                    }
                    NameBinding::Ambiguous(ids) if !ids.contains(&id) => ids.push(id),
                    _ => {}
                },
            }
        }
        Self { entries }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve one `[params]` key to an id.
    ///
    /// # Errors
    ///
    /// Unknown names (with near-miss suggestions) and ambiguous
    /// names (with the candidate ids) return a human-readable
    /// message; callers prefix the file path.
    pub fn resolve(&self, key: &str) -> Result<u32, String> {
        match self.entries.get(key) {
            Some(NameBinding::Id(id)) => Ok(*id),
            Some(NameBinding::Ambiguous(ids)) => Err(format!(
                "param \"{key}\" is ambiguous (ids {ids:?} across nested params structs); \
                 use the numeric id"
            )),
            None => {
                let mut close: Vec<&str> = self
                    .entries
                    .keys()
                    .filter(|k| {
                        k.starts_with(key)
                            || key.starts_with(k.as_str())
                            || k.to_lowercase() == key.to_lowercase()
                    })
                    .map(String::as_str)
                    .collect();
                close.truncate(3);
                if close.is_empty() {
                    Err(format!("unknown param \"{key}\""))
                } else {
                    Err(format!("unknown param \"{key}\" (did you mean {close:?}?)"))
                }
            }
        }
    }
}

/// Render a canonical `.preset` TOML file - the inverse of
/// [`read_presets_dir`]'s per-file parse. Used by
/// `cargo truce preset pull` / `import` to materialize host-saved
/// presets into the authored library, with param lines annotated
/// from `annotations` when available.
///
/// `meta.category` is written as an explicit field only when
/// non-empty; callers placing the file inside a category directory
/// clear it to keep the directory-derived convention.
#[must_use]
pub fn render_preset_toml(
    meta: &PresetMeta,
    params: &[(u32, f64)],
    extra: &[u8],
    annotations: &std::collections::BTreeMap<u32, ParamAnnotation>,
) -> String {
    use base64::Engine as _;
    use std::fmt::Write as _;

    let mut out = String::new();
    let mut field = |key: &str, value: &str| {
        if !value.is_empty() {
            let _ = writeln!(out, "{key} = \"{}\"", toml_escape(value));
        }
    };
    field("uuid", &meta.uuid);
    field("name", &meta.name);
    field("category", &meta.category);
    field("author", &meta.author);
    field("comment", &meta.comment);
    if !meta.tags.is_empty() {
        let tags = meta
            .tags
            .iter()
            .map(|t| format!("\"{}\"", toml_escape(t)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "tags = [{tags}]");
    }
    if meta.default {
        out.push_str("default = true\n");
    }
    if !extra.is_empty() {
        let _ = writeln!(
            out,
            "extra = \"{}\"",
            base64::engine::general_purpose::STANDARD.encode(extra)
        );
    }

    if !params.is_empty() {
        // Field-identifier keys when the sidecars provide them (and
        // the name maps back to exactly this id); ids otherwise.
        let names = ParamNameMap::from_annotations(annotations);
        out.push_str("\n[params]\n");
        for (id, value) in params {
            let annotation = annotations.get(id);
            let key = annotation
                .map(|a| a.field.as_str())
                .filter(|f| !f.is_empty() && names.resolve(f) == Ok(*id))
                .map_or_else(|| id.to_string(), str::to_string);
            let _ = write!(out, "{key} = {value}");
            if let Some(a) = annotation {
                if a.unit.is_empty() {
                    let _ = write!(out, "   # {}", a.name);
                } else {
                    let _ = write!(out, "   # {} ({})", a.name, a.unit);
                }
            }
            out.push('\n');
        }
    }
    out
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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

        let presets = read_presets_dir(&dir, false, None).unwrap();
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

        assert!(read_presets_dir(&dir, false, None).is_err());

        let first = read_presets_dir(&dir, true, None).unwrap();
        let stamped = first[0].meta.uuid.clone();
        assert_eq!(stamped.len(), 36);
        assert_eq!(&stamped[14..15], "4");

        // Second read sees the same identity from the rewritten file.
        let second = read_presets_dir(&dir, false, None).unwrap();
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
        let err = read_presets_dir(&dir, false, None).unwrap_err();
        assert!(err.contains("duplicate preset uuid"));

        std::fs::write(
            dir.join("b.preset"),
            "name = \"B\"\nuuid = \"v\"\ndefault = true\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false, None).unwrap_err();
        assert!(err.contains("default = true"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_round_trips_through_parser() {
        let dir = temp_lib("render");
        let meta = truce_utils::preset::PresetMeta {
            uuid: "u-render".into(),
            name: "Pulled \"Lead\"".into(),
            category: String::new(),
            author: "DAW".into(),
            comment: String::new(),
            tags: vec!["pulled".into()],
            default: false,
        };
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert(
            1,
            ParamAnnotation {
                field: "cutoff".into(),
                name: "Cutoff".into(),
                unit: "Hz".into(),
            },
        );
        let toml = render_preset_toml(&meta, &[(0, 1.0), (1, 8200.0)], b"xs", &annotations);
        assert!(toml.contains("cutoff = 8200   # Cutoff (Hz)"));
        std::fs::create_dir_all(dir.join("lead")).unwrap();
        std::fs::write(dir.join("lead/pulled.preset"), &toml).unwrap();

        let names = ParamNameMap::from_annotations(&annotations);
        let presets = read_presets_dir(&dir, false, Some(&names)).unwrap();
        let p = &presets[0];
        assert_eq!(p.meta.uuid, "u-render");
        assert_eq!(p.meta.name, "Pulled \"Lead\"");
        assert_eq!(p.meta.category, "lead"); // directory-derived
        let mut params = p.params.clone();
        params.sort_by_key(|(id, _)| *id);
        assert_eq!(params, vec![(0, 1.0), (1, 8200.0)]);
        assert_eq!(p.extra, b"xs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_field_name_keys() {
        let mut annotations = std::collections::BTreeMap::new();
        let a = |field: &str, name: &str| ParamAnnotation {
            field: field.into(),
            name: name.into(),
            unit: String::new(),
        };
        annotations.insert(0, a("waveform", "Waveform"));
        annotations.insert(1, a("cutoff", "Filter Cutoff"));
        annotations.insert(2, a("attack", "Attack"));
        annotations.insert(3, a("attack", "Attack (sub)")); // nested twin
        let names = ParamNameMap::from_annotations(&annotations);

        let dir = temp_lib("names");
        std::fs::write(
            dir.join("a.preset"),
            "name = \"A\"\nuuid = \"u\"\n[params]\ncutoff = 8200.0\n0 = 1\n",
        )
        .unwrap();
        let presets = read_presets_dir(&dir, false, Some(&names)).unwrap();
        let mut params = presets[0].params.clone();
        params.sort_by_key(|(id, _)| *id);
        assert_eq!(params, vec![(0, 1.0), (1, 8200.0)]);

        // Unknown name errors with a suggestion.
        std::fs::write(
            dir.join("a.preset"),
            "name = \"A\"\nuuid = \"u\"\n[params]\ncutof = 1.0\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false, Some(&names)).unwrap_err();
        assert!(err.contains("did you mean"), "{err}");

        // Ambiguous nested field errors with the candidates.
        std::fs::write(
            dir.join("a.preset"),
            "name = \"A\"\nuuid = \"u\"\n[params]\nattack = 0.1\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false, Some(&names)).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");

        // Name + id double-setting the same param errors.
        std::fs::write(
            dir.join("a.preset"),
            "name = \"A\"\nuuid = \"u\"\n[params]\ncutoff = 1.0\n1 = 2.0\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false, Some(&names)).unwrap_err();
        assert!(err.contains("set twice"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_uses_field_keys() {
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert(
            1,
            ParamAnnotation {
                field: "cutoff".into(),
                name: "Filter Cutoff".into(),
                unit: "Hz".into(),
            },
        );
        let meta = truce_utils::preset::PresetMeta {
            uuid: "u".into(),
            name: "N".into(),
            ..Default::default()
        };
        let toml = render_preset_toml(&meta, &[(1, 8200.0), (5, 0.5)], &[], &annotations);
        assert!(
            toml.contains("cutoff = 8200   # Filter Cutoff (Hz)"),
            "{toml}"
        );
        assert!(toml.contains("5 = 0.5"), "{toml}"); // no annotation -> id key
    }

    #[test]
    fn rejects_bad_params() {
        let dir = temp_lib("badparams");
        std::fs::write(
            dir.join("a.preset"),
            "name = \"A\"\nuuid = \"u\"\n[params]\ncutoff = 1.0\n",
        )
        .unwrap();
        let err = read_presets_dir(&dir, false, None).unwrap_err();
        assert!(err.contains("no param manifest is"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
