//! Format-neutral build-time plugin metadata.
//!
//! `#[derive(Params)]` and `#[derive(ParamEnum)]` write one *sidecar*
//! per type into `<target>/truce-meta/<crate>/`, and `truce::plugin!`
//! writes a `root.toml` naming the root params type. Neither renders
//! anything: the sidecars are inert data, and every format reads them
//! at package time through [`resolve_root`].
//!
//! ```text
//! target/truce-meta/<crate>/
//!   schema                  # "1" - this directory is a contract
//!   root.toml               # root params type + crate, from plugin!
//!   <Struct>.params.toml    # one per derive(Params)
//!   <Enum>.enum.toml        # one per derive(ParamEnum)
//! ```
//!
//! [`resolve_root`] walks that tree from the root struct, flattening
//! `#[nested]` groups and arrays into one id-resolved parameter list.
//! The walk is the half of the old compile-time LV2 emitter that was
//! never LV2-specific - it reproduces the runtime's id arithmetic
//! (`truce_params::{param_id_hash, nested_elem_salt, mix_nested_id}`)
//! and its parameter naming (`truce_utils::field_label`), so a
//! rendered port and the live plugin agree on both.
//!
//! What stays out: every format's own vocabulary. A [`ResolvedParam`]
//! carries `range` / `unit` / `flags` as the *raw* attribute strings
//! the sidecar recorded, because what a range shape means is a
//! question for whoever renders it. LV2 degrades a skewed taper to its
//! linear bounds and rejects a `const` bound outright; another format
//! may do neither. Keeping the strings opaque here is what lets a
//! format live outside this repo.
//!
//! Because this directory is read by out-of-tree code, its layout
//! changes by [`SCHEMA`] bump, never in place.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use truce_params::METER_ID_BASE;

/// Version of the `truce-meta` directory layout. Written to `schema`
/// and recorded in `root.toml`; a reader that finds a number it
/// doesn't know must fail rather than guess.
pub const SCHEMA: u32 = 1;

/// Directory under `target/` holding the metadata tree.
pub const DIR: &str = "truce-meta";

/// `<target>/truce-meta/<pkg>` - where one plugin crate's sidecars live.
#[must_use]
pub fn meta_dir(target_dir: &Path, pkg: &str) -> PathBuf {
    target_dir.join(DIR).join(pkg)
}

/// What `truce::plugin!` records about the plugin's root: enough to
/// start the [`resolve_root`] walk, and nothing that `truce.toml` or
/// cargo metadata already answers (duplicating those would create a
/// second source of truth for the plugin's identity and version).
#[derive(Debug, Clone)]
pub struct RootInfo {
    pub schema: u32,
    /// Bare name of the root `Params` type, as written at the
    /// `truce::plugin!` call site.
    pub root: String,
    /// Plugin crate that expanded the macro.
    pub crate_name: String,
}

/// Render `root.toml`.
#[must_use]
pub fn render_root(root_struct: &str, crate_name: &str) -> String {
    let mut buf = String::new();
    let _ = writeln!(buf, "schema = {SCHEMA}");
    let _ = writeln!(buf, "root = \"{}\"", crate::toml_escape(root_struct));
    let _ = writeln!(buf, "crate = \"{}\"", crate::toml_escape(crate_name));
    buf
}

/// The remedy for a tree missing a file its plugin's build should have
/// written. Cargo doesn't track the files a proc macro writes, so once
/// `target/truce-meta` is cleared (or a schema bump empties it), a cached
/// build never reruns the derives that would refill it - the build
/// "succeeds" and the tree stays empty until the crate recompiles.
fn rebuild_hint(meta_dir: &Path) -> String {
    let krate = meta_dir.file_name().map_or_else(
        || "<crate>".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    format!(
        "If the plugin built fine, `{DIR}` was likely cleared while its \
         build stayed cached - cargo doesn't rerun a derive it has \
         cached, so nothing rewrites it. Run `cargo clean -p {krate}`, \
         then build again."
    )
}

/// Read `root.toml` back.
///
/// # Errors
///
/// When the plugin hasn't been compiled since this contract landed, or
/// when its schema is one this build doesn't understand.
pub fn read_root(meta_dir: &Path) -> Result<RootInfo, String> {
    let path = meta_dir.join("root.toml");
    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "no plugin metadata at {}: {e}. `truce::plugin!` writes it \
             while the plugin crate compiles, so build the plugin first. \
             {}",
            path.display(),
            rebuild_hint(meta_dir)
        )
    })?;
    let doc: toml::Table = content
        .parse()
        .map_err(|e| format!("malformed {}: {e}", path.display()))?;
    let schema = doc
        .get("schema")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("{}: missing `schema`", path.display()))?;
    if schema != SCHEMA {
        return Err(format!(
            "{}: metadata schema {schema}, but this build of truce reads \
             schema {SCHEMA}. Rebuild the plugin with a matching truce.",
            path.display()
        ));
    }
    let string = |key: &str| {
        doc.get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{}: missing `{key}`", path.display()))
    };
    Ok(RootInfo {
        schema,
        root: string("root")?,
        crate_name: string("crate")?,
    })
}

/// One parameter, with its id already folded into the plugin's id
/// space and its name already qualified by any enclosing `#[nested]`
/// group or array.
///
/// `range`, `unit` and `flags` are the raw `#[param(...)]` attribute
/// strings; see the module docs for why they stay unparsed.
#[derive(Debug, Clone, Default)]
pub struct ResolvedParam {
    /// Id in the plugin's flat id space (nested bases already applied).
    pub id: u32,
    /// Display name, qualified by enclosing group / array labels.
    pub name: String,
    /// Rust field identifier - the stable key `.preset` files resolve
    /// parameter names through.
    pub field: String,
    /// `"Float"`, `"Bool"`, `"Int"` or `"Enum"`.
    pub kind: String,
    /// Raw range attribute (`"linear(0, 1)"`, `"enum(4)"`, `""`).
    pub range: String,
    /// The range refers to a `const`, so it has no build-time value.
    pub range_const: bool,
    /// Literal default, or `None` when [`Self::default_const`] is set.
    pub default: Option<f64>,
    /// The default refers to a `const`, so it has no build-time value.
    pub default_const: bool,
    /// Raw unit token (`"dB"`, `"Hz"`, `""`).
    pub unit: String,
    /// Raw `|`-separated flag tokens (`"bypass|readonly"`, `""`).
    pub flags: String,
    /// Raw MIDI binding (`"cc(7)"`, `"pitchbend"`), if declared.
    pub midi_binding: Option<String>,
    /// Wire channel `0..=15`, or `None` for any channel.
    pub midi_channel: Option<u8>,
    /// Variant count for an `Enum` param whose `<T>.enum.toml` sidecar
    /// resolved. `None` when it didn't (a cross-crate enum).
    pub enum_count: Option<u32>,
    /// Variant display names in index order; empty when unresolved.
    pub enum_names: Vec<String>,
}

/// A plugin's fully flattened parameter and meter metadata.
#[derive(Debug, Clone, Default)]
pub struct ResolvedRoot {
    pub params: Vec<ResolvedParam>,
    pub meter_ids: Vec<u32>,
}

/// Walk the metadata tree from `root_struct`, flattening every
/// `#[nested]` group and array into one id-resolved parameter list.
///
/// Reproduces the runtime's id arithmetic and parameter naming exactly;
/// see `walk` for the correspondence, which is load-bearing (a port's
/// name derives its `lv2:symbol`, and hosts persist those in saved
/// sessions).
///
/// # Errors
///
/// When a struct the tree references is missing - the plugin was built
/// against an older derive, or the tree was partially deleted.
pub fn resolve_root(meta_dir: &Path, root_struct: &str) -> Result<ResolvedRoot, String> {
    let mut out = ResolvedRoot::default();
    walk(
        meta_dir,
        root_struct,
        0,
        &mut out,
        &mut HashSet::new(),
        false,
    )?;
    Ok(out)
}

/// Recursively walk `<struct_name>.params.toml` and each `[[nested]]`
/// reference, appending to `out`.
///
/// Returns, per parameter this call appended, whether it sits under a
/// `#[nested]` array (its name is array-qualified) - a parent single
/// group prefixes its label onto exactly those, as the runtime does.
fn walk(
    meta_dir: &Path,
    struct_name: &str,
    id_base: u32,
    out: &mut ResolvedRoot,
    ancestors: &mut HashSet<String>,
    qualify: bool,
) -> Result<Vec<bool>, String> {
    // Track the active path, not every visited struct: the same Params
    // type reused in two `#[nested]` slots must be walked twice, but a
    // true cycle (a type nesting an ancestor) still has to be rejected.
    if !ancestors.insert(struct_name.to_string()) {
        return Err(format!(
            "cyclic #[nested] reference through `{struct_name}`"
        ));
    }
    let path = meta_dir.join(format!("{struct_name}.params.toml"));
    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "no metadata sidecar at {}: {e}. derive(Params) writes one \
             for each Params struct while the crate compiles. If \
             `{struct_name}` lives in another crate, that's the cause: \
             cross-crate #[nested] is unsupported. {}",
            path.display(),
            rebuild_hint(meta_dir)
        )
    })?;
    let doc: toml::Table = content
        .parse()
        .map_err(|e| format!("malformed {}: {e}", path.display()))?;

    // `scheme` mirrors the struct's `IdScheme` (default hash). It picks
    // how an un-pinned nested group is based, exactly as the runtime
    // `offset_ids` does, so this flattened table agrees with
    // `param_infos()`.
    let hash_scheme = doc.get("scheme").and_then(toml::Value::as_str) != Some("ordinal");
    let mask = METER_ID_BASE - 1;

    // Own params carry struct-local ids; fold them into the parent's id
    // space by `id_base` (additive + mask, matching `offset_ids`).
    let mut own_count = 0u32;
    let mut under_array: Vec<bool> = Vec::new();
    if let Some(toml::Value::Array(arr)) = doc.get("param") {
        for entry in arr {
            let mut p =
                parse_param(entry, meta_dir).map_err(|e| format!("{}: {e}", path.display()))?;
            p.id = (p.id + id_base) & mask;
            out.params.push(p);
            under_array.push(false);
            own_count += 1;
        }
    }
    if let Some(toml::Value::Array(arr)) = doc.get("meter") {
        for entry in arr {
            if let Some(id) = entry.get("id").and_then(toml::Value::as_integer)
                && let Ok(id) = u32::try_from(id)
            {
                out.meter_ids.push(id);
            }
        }
    }
    if let Some(toml::Value::Array(arr)) = doc.get("nested") {
        // Ordinal: `next_base` packs each auto group after the previous
        // one. Hash: each group's base is a stable hash of its slot
        // field name (no packing). An explicit `base` pins either.
        let mut next_base = own_count;
        for entry in arr {
            let Some(child) = entry.get("type").and_then(toml::Value::as_str) else {
                continue;
            };
            let slot_field = entry.get("field").and_then(toml::Value::as_str);
            let auto_base = if hash_scheme {
                slot_field.map_or(0, truce_params::param_id_hash)
            } else {
                next_base
            };
            let base = entry
                .get("base")
                .and_then(toml::Value::as_integer)
                .and_then(|b| u32::try_from(b).ok())
                .unwrap_or(auto_base);
            // A single `#[nested]` writes no `array_len` and walks once;
            // an array writes its literal length (`0` walks zero times).
            // A const length (`[T; MAX_BANDS]`) is recorded as written:
            // the derive can't evaluate it, and neither can this walk, which
            // runs without the plugin.
            if let Some(len) = entry.get("array_len_const").and_then(toml::Value::as_str) {
                return Err(format!(
                    "`{struct_name}.{}` is a #[nested] array of const length `{len}`, which \
                     build metadata can't evaluate without running the plugin. The plugin \
                     works on every format that reads its parameters at runtime; to package \
                     it for one that needs them at build time, write the length as a literal.",
                    slot_field.unwrap_or(child),
                ));
            }
            let array = entry
                .get("array_len")
                .and_then(toml::Value::as_integer)
                .and_then(|n| usize::try_from(n).ok());
            let label = entry
                .get("label")
                .and_then(toml::Value::as_str)
                .map(str::to_string)
                .or_else(|| slot_field.map(truce_utils::field_label))
                .unwrap_or_default();
            let mut elem_base = base;
            for i in 0..array.unwrap_or(1) {
                let before = out.params.len();
                // Hash: each element's subtree is walked at base 0, then
                // every id is mixed with the element's salt and only then
                // offset by this struct's `id_base` - the runtime's order
                // (the element's `remix_ids`, then any ancestor's additive
                // `offset_ids`). The derive rejects a pinned base there.
                // Ordinal: at `base + i * element_count`, the count
                // discovered by how many params the walk added.
                let salt = match (hash_scheme, array.is_some(), slot_field) {
                    (true, true, Some(f)) => Some(truce_params::nested_elem_salt(
                        truce_params::field_hash(f),
                        u32::try_from(i).unwrap_or(u32::MAX),
                    )),
                    _ => None,
                };
                let walk_base = if salt.is_some() {
                    0
                } else {
                    (id_base + elem_base) & mask
                };
                // Match the runtime's naming: an array element is walked
                // qualified (its single groups prefix their label), and so
                // is a single group that is itself inside an element.
                let sub_under_array = walk(
                    meta_dir,
                    child,
                    walk_base,
                    out,
                    ancestors,
                    qualify || array.is_some(),
                )?;
                if let Some(salt) = salt {
                    for p in &mut out.params[before..] {
                        p.id = (truce_params::mix_nested_id(p.id, salt) + id_base) & mask;
                    }
                }
                // Match the runtime's `param_infos()` (`gen_infos_appends`):
                // an array element's names get the array's label and 1-based
                // index (`Level` -> `Step 1 Level`); a single group's get its
                // label when walked qualified (inside an element), and
                // otherwise only on the names that came from an array below
                // it. The port name - and the `lv2:symbol` derived from it -
                // must agree, and symbols are persisted in host sessions, so
                // they can't change later.
                if array.is_some() {
                    for p in &mut out.params[before..] {
                        p.name = format!("{label} {} {}", i + 1, p.name);
                    }
                    under_array.extend(std::iter::repeat_n(true, sub_under_array.len()));
                } else {
                    for (p, &arr) in out.params[before..].iter_mut().zip(&sub_under_array) {
                        if qualify || arr {
                            p.name = format!("{label} {}", p.name);
                        }
                    }
                    under_array.extend(sub_under_array);
                }
                let added = u32::try_from(out.params.len() - before).unwrap_or(0);
                elem_base = elem_base.wrapping_add(added);
                // Runtime `offset_ids` packs each auto group at `own_count +
                // Σ count(previous nested)` - it sums the previous groups'
                // sizes and ignores their explicit bases. Accumulate `added`
                // from the running total (not `base + added`), or an explicit
                // `base` on an earlier group would shift every later auto
                // group's ids away from where the live plugin puts them.
                next_base += added;
            }
        }
    }
    ancestors.remove(struct_name);
    Ok(under_array)
}

/// Parse one `[[param]]` sidecar entry. Range / unit / flags pass
/// through as written; only the enum sidecar is resolved, because a
/// variant count is a `truce` concept rather than any one format's.
fn parse_param(v: &toml::Value, meta_dir: &Path) -> Result<ResolvedParam, String> {
    let id = v
        .get("id")
        .and_then(toml::Value::as_integer)
        .and_then(|i| u32::try_from(i).ok())
        .ok_or("[[param]].id missing or out of range")?;
    let string = |key: &str| {
        v.get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let flag = |key: &str| v.get(key).and_then(toml::Value::as_bool) == Some(true);
    let enum_type = string("enum_type");
    let kind = v
        .get("kind")
        .and_then(toml::Value::as_str)
        .unwrap_or("Float")
        .to_string();
    // Only an `Enum` param consults the enum sidecar; a numeric param
    // with an `enum(N)` range carries its own count in `range`.
    let (enum_count, enum_names) = if kind == "Enum" {
        read_enum(meta_dir, &enum_type)
    } else {
        (None, Vec::new())
    };
    Ok(ResolvedParam {
        id,
        name: string("name"),
        field: string("field"),
        kind,
        range: string("range"),
        range_const: flag("range_const"),
        // The sidecar writer always emits `default` unless it is a
        // const; `None` here otherwise means an out-of-tree producer
        // skipped the field, which every consumer treats as 0.0 (the
        // runtime's own implicit default).
        default: v.get("default").and_then(toml_f64),
        default_const: flag("default_const"),
        unit: string("unit"),
        flags: string("flags"),
        midi_binding: v
            .get("midi_binding")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        midi_channel: v
            .get("midi_channel")
            .and_then(toml::Value::as_integer)
            .and_then(|i| u8::try_from(i).ok()),
        enum_count,
        enum_names,
    })
}

/// Variant count and display names from the `<Enum>.enum.toml` sidecar
/// `derive(ParamEnum)` writes. `(None, [])` when the type isn't
/// recorded or the sidecar is absent (a cross-crate enum) - the caller
/// decides whether that is fatal.
fn read_enum(meta_dir: &Path, enum_type: &str) -> (Option<u32>, Vec<String>) {
    if enum_type.is_empty() {
        return (None, Vec::new());
    }
    let Ok(content) = std::fs::read_to_string(meta_dir.join(format!("{enum_type}.enum.toml")))
    else {
        return (None, Vec::new());
    };
    let Ok(doc) = content.parse::<toml::Table>() else {
        return (None, Vec::new());
    };
    let count = doc
        .get("count")
        .and_then(toml::Value::as_integer)
        .and_then(|c| u32::try_from(c).ok());
    let names = doc
        .get("names")
        .and_then(toml::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (count, names)
}

fn toml_f64(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Float(f) => Some(*f),
        #[allow(clippy::cast_precision_loss)]
        toml::Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolvedRoot, resolve_root};

    /// Write a throwaway metadata tree and resolve it.
    fn try_resolve(name: &str, files: &[(&str, &str)]) -> Result<ResolvedRoot, String> {
        let dir = std::env::temp_dir().join(format!("truce-meta-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (ty, body) in files {
            std::fs::write(dir.join(format!("{ty}.params.toml")), body).unwrap();
        }
        let out = resolve_root(&dir, "Root");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn resolve(name: &str, files: &[(&str, &str)]) -> ResolvedRoot {
        try_resolve(name, files).unwrap()
    }

    const STEP: &str = "scheme = \"ordinal\"\n\
         [[param]]\nid = 0\nname = \"Level\"\nfield = \"level\"\nkind = \"Float\"\n\
         range = \"linear(0, 1)\"\ndefault = 1\n";

    /// `#[nested]` array elements must carry the same names as at
    /// runtime (`Bank 1 Step 2 Level`: each array's label and 1-based
    /// index, outermost first). A root-level single group stays raw;
    /// one inside an element gets its label.
    #[test]
    fn nested_array_names_match_runtime() {
        let got = resolve(
            "nested",
            &[
                (
                    "Root",
                    "scheme = \"ordinal\"\n\
                     [[nested]]\ntype = \"Bank\"\nfield = \"bank\"\narray_len = 2\n\
                     [[nested]]\ntype = \"Step\"\nfield = \"solo\"\n",
                ),
                (
                    "Bank",
                    "scheme = \"ordinal\"\n\
                     [[nested]]\ntype = \"Step\"\nfield = \"step\"\narray_len = 2\n\
                     [[nested]]\ntype = \"Step\"\nfield = \"main_env\"\n",
                ),
                ("Step", STEP),
            ],
        );
        let names: Vec<&str> = got.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Bank 1 Step 1 Level",
                "Bank 1 Step 2 Level",
                "Bank 1 Main Env Level",
                "Bank 2 Step 1 Level",
                "Bank 2 Step 2 Level",
                "Bank 2 Main Env Level",
                "Level",
            ]
        );
    }

    /// A single group outside any array keeps its own params raw but
    /// prefixes its label onto names from an array below it, so the
    /// same group type in two slots doesn't repeat `Harm 1 Level`.
    #[test]
    fn reused_group_prefixes_only_its_array_names() {
        let got = resolve(
            "reused",
            &[
                (
                    "Root",
                    "scheme = \"ordinal\"\n\
                     [[nested]]\ntype = \"Holder\"\nfield = \"osc_a\"\nlabel = \"Osc A\"\n\
                     [[nested]]\ntype = \"Holder\"\nfield = \"osc_b\"\nlabel = \"Osc B\"\n",
                ),
                (
                    "Holder",
                    "scheme = \"ordinal\"\n\
                     [[param]]\nid = 0\nname = \"Gain\"\nfield = \"gain\"\nkind = \"Float\"\n\
                     range = \"linear(0, 1)\"\ndefault = 1\n\
                     [[nested]]\ntype = \"Leaf\"\nfield = \"harm\"\nlabel = \"Harm\"\narray_len = 2\n",
                ),
                ("Leaf", STEP),
            ],
        );
        let names: Vec<&str> = got.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Gain",
                "Osc A Harm 1 Level",
                "Osc A Harm 2 Level",
                "Gain",
                "Osc B Harm 1 Level",
                "Osc B Harm 2 Level",
            ]
        );
    }

    /// Hash-scheme arrays place each element's ids by `mix_nested_id`
    /// with its `nested_elem_salt`, matching the runtime, rather than
    /// packing at `base + i * element_count`.
    #[test]
    fn hash_nested_array_mixes_per_element_salt() {
        let got = resolve(
            "hash-array",
            &[
                (
                    "Root",
                    "scheme = \"hash\"\n\
                     [[nested]]\ntype = \"Step\"\nfield = \"step\"\narray_len = 3\n",
                ),
                (
                    "Step",
                    "scheme = \"hash\"\n\
                     [[param]]\nid = 7\nname = \"Level\"\nfield = \"level\"\nkind = \"Float\"\n\
                     range = \"linear(0, 1)\"\ndefault = 1\n",
                ),
            ],
        );
        let ids: Vec<u32> = got.params.iter().map(|p| p.id).collect();
        let want: Vec<u32> = (0..3)
            .map(|i| {
                truce_params::mix_nested_id(
                    7,
                    truce_params::nested_elem_salt(truce_params::field_hash("step"), i),
                )
            })
            .collect();
        assert_eq!(ids, want);
    }

    /// A const array length can't be walked - ordinal ids after it, and
    /// the element count itself, depend on its value - so the walk names
    /// the field and the const instead of guessing a length.
    #[test]
    fn const_nested_array_length_is_refused_by_name() {
        let err = try_resolve(
            "const-array",
            &[
                (
                    "Root",
                    "scheme = \"hash\"\n\
                     [[nested]]\ntype = \"Step\"\nfield = \"step\"\n\
                     array_len_const = \"MAX_STEPS\"\n",
                ),
                ("Step", STEP),
            ],
        )
        .expect_err("a const length must not resolve");
        assert!(
            err.contains("`Root.step`") && err.contains("`MAX_STEPS`"),
            "{err}"
        );
    }

    /// A cycle through `#[nested]` is rejected rather than recursed
    /// into, but the same type in two sibling slots is walked twice.
    #[test]
    fn cycles_rejected_reuse_allowed() {
        let dir = std::env::temp_dir().join(format!("truce-meta-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Root.params.toml"),
            "scheme = \"ordinal\"\n[[nested]]\ntype = \"Loop\"\nfield = \"a\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Loop.params.toml"),
            "scheme = \"ordinal\"\n[[nested]]\ntype = \"Root\"\nfield = \"b\"\n",
        )
        .unwrap();
        let err = resolve_root(&dir, "Root").unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("cyclic"), "{err}");
    }

    /// Raw attribute strings reach the caller unparsed - including a
    /// `const` marker, which this layer records rather than judges.
    #[test]
    fn raw_attributes_pass_through() {
        let got = resolve(
            "raw",
            &[(
                "Root",
                "scheme = \"ordinal\"\n\
                 [[param]]\nid = 0\nname = \"Cut\"\nfield = \"cut\"\nkind = \"Float\"\n\
                 range = \"skewed(20, 20000, 0.3)\"\nunit = \"Hz\"\nflags = \"readonly\"\n\
                 range_const = true\ndefault_const = true\n\
                 midi_binding = \"cc(74)\"\nmidi_channel = 3\n",
            )],
        );
        let p = &got.params[0];
        assert_eq!(p.range, "skewed(20, 20000, 0.3)");
        assert_eq!(p.unit, "Hz");
        assert_eq!(p.flags, "readonly");
        assert!(p.range_const && p.default_const);
        assert_eq!(p.default, None);
        assert_eq!(p.midi_binding.as_deref(), Some("cc(74)"));
        assert_eq!(p.midi_channel, Some(3));
    }
}
