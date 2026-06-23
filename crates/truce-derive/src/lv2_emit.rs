//! Compile-time LV2 metadata emission.
//!
//! Two cooperating proc-macros wire this up:
//!
//! 1. `derive(Params)` writes a *per-struct sidecar* at
//!    `<target>/lv2-meta/<crate>/<struct>.params.toml` with this
//!    struct's own params, meters, and `#[nested]` child type names.
//!    No TTL rendered here - every `derive(Params)` invocation in
//!    every crate writes its own sidecar, including helper / utility /
//!    test structs. Cheap metadata; no side effects beyond the file.
//!
//! 2. `__truce_lv2_emit_root!(<params_type>)` is invoked by
//!    `truce::plugin!` with the root params type. It reads the root
//!    sidecar, recursively walks `[[nested]]` references to aggregate
//!    every param + meter, looks up plugin info from `truce.toml`,
//!    and renders the final `manifest.ttl` / `plugin.ttl` next to the
//!    sidecars.
//!
//! Step 1 fires for non-plugin crates too, which is fine - those
//! sidecars are unused unless something invokes `__truce_lv2_emit_root!`
//! on them. Step 2 only fires once per plugin (from `truce::plugin!`).
//!
//! Limitations:
//! - Cross-crate `#[nested]` (a nested Params type from a different
//!   crate) is unsupported: the aggregator looks for sidecars in the
//!   plugin's own `<target>/lv2-meta/<crate>/` directory and errors
//!   out if a referenced type is missing.
//! - Audio in/out counts default to category-derived stereo. Plugins
//!   that override `Plugin::bus_layouts()` get the wrong port count
//!   in the TTL until `audio_in` / `audio_out` are added to
//!   `[[plugin]]` in `truce.toml`.

use crate::{MeterField, ParamField};
use proc_macro::TokenStream;
use quote::quote;
use std::fmt::Write as _;
use std::path::PathBuf;
use syn::Type;
use truce_build::lv2::Lv2Param;

/// Write the per-struct sidecar. Best-effort; errors don't fail the
/// build (a missing sidecar will surface later when
/// `__truce_lv2_emit_root!` aggregates).
pub(crate) fn write_struct_sidecar(
    struct_name: &syn::Ident,
    params: &[ParamField],
    meters: &[MeterField],
    nested: &[(syn::Ident, Type, Option<u32>)],
) {
    let Some(out_dir) = sidecar_dir() else {
        return;
    };
    if std::fs::create_dir_all(&out_dir).is_err() {
        return;
    }
    let mut buf = String::new();
    let _ = writeln!(buf, "struct = \"{struct_name}\"\n");
    for p in params {
        buf.push_str("[[param]]\n");
        let _ = writeln!(buf, "id = {}", p.id());
        // The Rust field identifier: the stable, bare-TOML-safe key
        // `.preset` files resolve param names through.
        let _ = writeln!(buf, "field = \"{}\"", toml_escape(&p.ident.to_string()));
        let name = p.attrs.name.clone().unwrap_or_else(|| p.ident.to_string());
        let _ = writeln!(buf, "name = \"{}\"", toml_escape(&name));
        let _ = writeln!(buf, "kind = \"{}\"", param_kind_str(p.kind));
        if let Some(r) = &p.attrs.range {
            let _ = writeln!(buf, "range = \"{}\"", toml_escape(r));
        } else if matches!(p.kind, crate::ParamKind::Enum)
            && let Some(ty) = p.enum_type()
            && let Some(seg) = type_last_segment(ty)
        {
            // No explicit `enum(N)`: record the enum's bare type name so
            // the aggregator can resolve its variant count from the
            // `<Enum>.enum.toml` sidecar `derive(ParamEnum)` wrote.
            let _ = writeln!(buf, "enum_type = \"{}\"", toml_escape(&seg));
        }
        // Always emit `default` so the LV2 TTL matches the runtime
        // `ParamInfo::default_plain`. The runtime falls back to `0.0`
        // when `#[param(default = ...)]` is omitted (see
        // `gen_param_info_literal`); the LV2 sidecar has to mirror
        // that, otherwise hosts read `lv2:default` from the TTL and
        // open the plugin at the range's minimum (e.g. gain at -60 dB)
        // while VST3 / standalone honour the runtime's 0.0.
        let default = p.attrs.default.as_ref().map_or(0.0, |d| d.value);
        let _ = writeln!(buf, "default = {default}");
        if let Some(u) = &p.attrs.unit {
            let _ = writeln!(buf, "unit = \"{}\"", toml_escape(u));
        }
        if let Some(f) = &p.attrs.flags {
            let _ = writeln!(buf, "flags = \"{}\"", toml_escape(f));
        }
        buf.push('\n');
    }
    for m in meters {
        if let Some(id) = m.id {
            buf.push_str("[[meter]]\n");
            let _ = writeln!(buf, "id = {id}\n");
        }
    }
    for (field, ty, base) in nested {
        if let Some(t) = type_last_segment(ty) {
            buf.push_str("[[nested]]\n");
            let _ = writeln!(buf, "type = \"{t}\"");
            // The field identifier preserves declaration order and lets
            // the aggregator name auto-packed groups deterministically.
            let _ = writeln!(buf, "field = \"{}\"", toml_escape(&field.to_string()));
            if let Some(b) = base {
                let _ = writeln!(buf, "base = {b}");
            }
            buf.push('\n');
        }
    }
    let _ = std::fs::write(out_dir.join(format!("{struct_name}.params.toml")), buf);
}

/// Record a `ParamEnum`'s variant count in a sidecar so the LV2
/// aggregator can resolve `EnumParam<T>` ports that carry no explicit
/// `#[param(range = "enum(N)")]`. `derive(ParamEnum)` is the only place
/// the variant count is known at proc-macro time (it counts the enum's
/// arms); the params sidecar records the bare enum type name, and
/// `parse_param_entry` reads the count back from `<Enum>.enum.toml`.
/// Without this the fallback range collapsed to `enum(0)`, rendering an
/// invalid `lv2:maximum 0` / `lv2:default 1` port that REAPER rejects.
/// Best-effort, mirroring [`write_struct_sidecar`].
pub(crate) fn write_enum_sidecar(enum_name: &syn::Ident, variant_count: usize) {
    let Some(out_dir) = sidecar_dir() else {
        return;
    };
    if std::fs::create_dir_all(&out_dir).is_err() {
        return;
    }
    let buf = format!("count = {variant_count}\n");
    let _ = std::fs::write(out_dir.join(format!("{enum_name}.enum.toml")), buf);
}

/// Implementation of `__truce_lv2_emit_root!(<params_type>)`. Walks
/// the params type's sidecar tree to render `manifest.ttl` /
/// `plugin.ttl`. Errors here surface as `compile_error!` tokens so the
/// plugin author sees them at build time.
pub(crate) fn emit_root_impl(input: TokenStream) -> TokenStream {
    // Accept any path-shaped input; `syn::Type` rejects bare paths
    // when the surrounding macro re-tokenizes them, while `syn::Path`
    // is the right shape for "the params type's name."
    let path: syn::Path = match syn::parse(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error().into(),
    };
    let Some(seg) = path.segments.last() else {
        return quote! { compile_error!("__truce_lv2_emit_root!: empty params path"); }.into();
    };
    let root_struct = seg.ident.to_string();

    // No plugin in truce.toml = no TTL needed (helper crate using the
    // macro for tests, or a workspace member that never ships).
    let Ok((config, pkg_name, truce_toml_path)) = crate::try_resolve_plugin() else {
        return TokenStream::new();
    };
    let Some(plugin) = config.plugin.iter().find(|p| p.crate_name == pkg_name) else {
        return TokenStream::new();
    };

    let Some(sidecar_dir) = sidecar_dir_for(&pkg_name, &truce_toml_path) else {
        return TokenStream::new();
    };

    let mut params: Vec<truce_build::lv2::Lv2Param> = Vec::new();
    let mut meter_ids: Vec<u32> = Vec::new();
    // Flattened `(global id, field)` for the install-time preset name
    // resolver, which can't see the nesting structure or the bases.
    let mut fields: Vec<(u32, String)> = Vec::new();
    let mut ancestors: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Err(msg) = aggregate(
        &sidecar_dir,
        &root_struct,
        0,
        &mut params,
        &mut meter_ids,
        &mut fields,
        &mut ancestors,
    ) {
        let lit = msg;
        return quote! { compile_error!(#lit); }.into();
    }

    let category = parse_category(&plugin.category);
    let (audio_in, audio_out) = audio_io_for(category);
    // Same `(accepts_midi_in, emits_midi)` derivation baked onto
    // `PluginInfo`, so the TTL ports and the runtime `PortLayout` agree.
    let (accepts_midi_in, has_midi_out) =
        truce_build::midi_capabilities(&plugin.category, plugin.midi_input, plugin.midi_output);

    let url = config.vendor.url.clone();
    let uri = truce_build::lv2::plugin_uri(&url, &plugin.bundle_id);
    let ui_uri = truce_build::lv2::ui_uri(&url, &plugin.bundle_id);

    let bundle = truce_build::lv2::Lv2Bundle {
        plugin_name: plugin.name.clone(),
        vendor: config.vendor.name.clone(),
        url,
        uri,
        ui_uri,
        category,
        audio_in,
        audio_out,
        accepts_midi_in,
        has_midi_out,
        params,
        meter_ids,
        // Always emit the UI block. Hosts that don't honour the UI
        // URI still load the plugin from manifest.ttl + plugin.ttl
        // and just skip the UI line.
        has_ui: true,
    };

    let slug = truce_utils::slugify(&plugin.name);
    // Windows loaders only resolve `.dll`; Linux/macOS LV2 bundles use `.so`.
    // Matches the extension `stage_lv2` writes into the bundle.
    let bin_ext = if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let bin_name = format!("{slug}.{bin_ext}");
    let (manifest_ttl, plugin_ttl) = truce_build::lv2::render_ttls(&bundle, &bin_name);

    let _ = std::fs::write(sidecar_dir.join("manifest.ttl"), &manifest_ttl);
    let _ = std::fs::write(sidecar_dir.join("plugin.ttl"), &plugin_ttl);
    let _ = std::fs::write(sidecar_dir.join("so_name.txt"), &bin_name);

    // Persist the resolved `id -> lv2:symbol` table so the install-time
    // preset emitter can write `lv2:port` / `pset:value` entries with
    // the exact symbols this manifest declared (collision resolution
    // needs the full param list, which only exists here).
    let symbols = truce_build::lv2::resolved_param_symbols(&bundle.params);
    let _ = std::fs::write(
        sidecar_dir.join("symbols.toml"),
        truce_build::presets::render_param_symbols(&symbols),
    );

    // Flattened `(global id, field, name)` index. The per-struct
    // sidecars hold struct-local ids, so a nested plugin's `[params]`
    // keys can't resolve from them; the preset name resolver reads this
    // single file, where every id is already in the plugin's id space.
    let mut index = String::new();
    for (id, field) in &fields {
        let name = bundle
            .params
            .iter()
            .find(|p| p.id == *id)
            .map_or("", |p| p.name.as_str());
        index.push_str("[[param]]\n");
        let _ = writeln!(index, "id = {id}");
        let _ = writeln!(index, "field = \"{}\"", toml_escape(field));
        let _ = writeln!(index, "name = \"{}\"", toml_escape(name));
        index.push('\n');
    }
    let _ = std::fs::write(sidecar_dir.join("param_index.toml"), index);

    TokenStream::new()
}

/// Recursively walk `<root>.params.toml`, then each `[[nested]]`
/// reference, accumulating params + meter IDs into the supplied vecs.
#[allow(clippy::too_many_arguments)]
fn aggregate(
    sidecar_dir: &std::path::Path,
    struct_name: &str,
    id_base: u32,
    params: &mut Vec<truce_build::lv2::Lv2Param>,
    meter_ids: &mut Vec<u32>,
    fields: &mut Vec<(u32, String)>,
    ancestors: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    // Track the active path, not every visited struct: the same Params
    // type reused in two `#[nested]` slots must be walked twice, but a
    // true cycle (a type nesting an ancestor) still has to be rejected.
    if !ancestors.insert(struct_name.to_string()) {
        return Err(format!(
            "cyclic #[nested] reference through `{struct_name}`"
        ));
    }
    let path = sidecar_dir.join(format!("{struct_name}.params.toml"));
    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "no LV2 sidecar at {}: {e}. derive(Params) writes one for \
             each Params struct during compile; missing it usually \
             means the type lives in another crate (cross-crate \
             #[nested] is unsupported).",
            path.display()
        )
    })?;
    let toml: toml::Table = content
        .parse()
        .map_err(|e| format!("malformed {}: {e}", path.display()))?;

    // Own params carry struct-local ids; shift them into the parent's id
    // space by `id_base`. Auto-packed nested groups start right after them.
    let mut own_count = 0u32;
    if let Some(toml::Value::Array(arr)) = toml.get("param") {
        for entry in arr {
            let mut p = parse_param_entry(entry, sidecar_dir)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            p.id += id_base;
            if let Some(field) = entry.get("field").and_then(toml::Value::as_str) {
                fields.push((p.id, field.to_string()));
            }
            params.push(p);
            own_count += 1;
        }
    }
    if let Some(toml::Value::Array(arr)) = toml.get("meter") {
        for entry in arr {
            if let Some(id) = entry.get("id").and_then(toml::Value::as_integer)
                && let Ok(id) = u32::try_from(id)
            {
                meter_ids.push(id);
            }
        }
    }
    if let Some(toml::Value::Array(arr)) = toml.get("nested") {
        // `next_base` packs each auto group after the previous one; an
        // explicit `base` pins the group and moves the cursor past it.
        let mut next_base = own_count;
        for entry in arr {
            let Some(t) = entry.get("type").and_then(toml::Value::as_str) else {
                continue;
            };
            let base = entry
                .get("base")
                .and_then(toml::Value::as_integer)
                .and_then(|b| u32::try_from(b).ok())
                .unwrap_or(next_base);
            let before = params.len();
            aggregate(
                sidecar_dir,
                t,
                id_base + base,
                params,
                meter_ids,
                fields,
                ancestors,
            )?;
            let added = u32::try_from(params.len() - before).unwrap_or(0);
            next_base = base + added;
        }
    }
    ancestors.remove(struct_name);
    Ok(())
}

fn parse_param_entry(v: &toml::Value, sidecar_dir: &std::path::Path) -> Result<Lv2Param, String> {
    let id = v
        .get("id")
        .and_then(toml::Value::as_integer)
        .and_then(|i| u32::try_from(i).ok())
        .ok_or("[[param]].id missing or out of range")?;
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("Float");
    let range_str = v.get("range").and_then(|x| x.as_str()).unwrap_or("");
    // The sidecar writer always emits `default`; the `NaN` sentinel
    // here is a defensive backstop for an out-of-tree producer that
    // skips the field.
    let default = v
        .get("default")
        .and_then(toml_value_to_f64)
        .unwrap_or(f64::NAN);
    let unit = v.get("unit").and_then(|x| x.as_str()).unwrap_or("");
    let flags = v.get("flags").and_then(|x| x.as_str()).unwrap_or("");
    let enum_type = v.get("enum_type").and_then(|x| x.as_str()).unwrap_or("");
    let range = parse_range_value(range_str, kind, enum_type, sidecar_dir)?;
    // Match `truce-derive::gen_param_info_literal`'s implicit default
    // (`a.default.unwrap_or(0.0)`) so the LV2 TTL agrees with the
    // ParamInfo VST3 / standalone read at runtime. The defensive
    // `NaN` branch only fires when the sidecar omits `default`
    // entirely (out-of-tree producer); the in-tree writer always
    // emits the field, so this normally lands in the `else` arm.
    let default_plain = if default.is_nan() { 0.0 } else { default };
    Ok(Lv2Param {
        id,
        name,
        default_plain,
        range,
        unit: parse_unit_value(unit),
        flags: parse_flags_value(flags),
    })
}

fn toml_value_to_f64(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Float(f) => Some(*f),
        #[allow(clippy::cast_precision_loss)]
        toml::Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

fn parse_range_value(
    s: &str,
    kind: &str,
    enum_type: &str,
    sidecar_dir: &std::path::Path,
) -> Result<truce_build::lv2::Lv2Range, String> {
    use truce_build::lv2::Lv2Range;
    if let Some(inner) = s.strip_prefix("linear(").and_then(|x| x.strip_suffix(')')) {
        let (lo, hi) = parse_pair_f64(inner)?;
        return Ok(Lv2Range::Linear { min: lo, max: hi });
    }
    if let Some(inner) = s.strip_prefix("log(").and_then(|x| x.strip_suffix(')')) {
        let (lo, hi) = parse_pair_f64(inner)?;
        return Ok(Lv2Range::Logarithmic { min: lo, max: hi });
    }
    if let Some(inner) = s
        .strip_prefix("discrete(")
        .and_then(|x| x.strip_suffix(')'))
    {
        let (lo, hi) = parse_pair_f64(inner)?;
        return Ok(Lv2Range::Discrete { min: lo, max: hi });
    }
    if let Some(inner) = s.strip_prefix("enum(").and_then(|x| x.strip_suffix(')')) {
        let count: u32 = inner
            .trim()
            .parse()
            .map_err(|e| format!("enum count: {e}"))?;
        return Ok(Lv2Range::Enum { count });
    }
    match kind {
        "Bool" => Ok(Lv2Range::Discrete { min: 0.0, max: 1.0 }),
        // A range-less `EnumParam<T>`: recover the variant count from
        // the `<T>.enum.toml` sidecar `derive(ParamEnum)` wrote. Falling
        // back to `count: 0` here is what produced the invalid
        // `lv2:maximum 0` / `lv2:default 1` port REAPER rejected, so only
        // do so when the sidecar is genuinely unresolvable (e.g. a
        // cross-crate enum) - the TTL renderer clamps the default into
        // range to keep even that case loadable.
        "Enum" => Ok(Lv2Range::Enum {
            count: enum_variant_count(enum_type, sidecar_dir).unwrap_or(0),
        }),
        _ => Err(format!("unrecognised range `{s}`")),
    }
}

/// Look up a `ParamEnum`'s variant count from the `<Enum>.enum.toml`
/// sidecar `derive(ParamEnum)` writes. `None` when the type name is
/// empty (no `enum_type` recorded) or the sidecar is absent (e.g. the
/// enum lives in another crate, which the aggregator can't reach).
fn enum_variant_count(enum_type: &str, sidecar_dir: &std::path::Path) -> Option<u32> {
    if enum_type.is_empty() {
        return None;
    }
    let path = sidecar_dir.join(format!("{enum_type}.enum.toml"));
    let content = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = content.parse().ok()?;
    table
        .get("count")
        .and_then(toml::Value::as_integer)
        .and_then(|c| u32::try_from(c).ok())
}

fn parse_pair_f64(s: &str) -> Result<(f64, f64), String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 2 {
        return Err(format!("expected two args, got `{s}`"));
    }
    let lo: f64 = parts[0].parse().map_err(|e| format!("lo: {e}"))?;
    let hi: f64 = parts[1].parse().map_err(|e| format!("hi: {e}"))?;
    Ok((lo, hi))
}

fn parse_unit_value(s: &str) -> truce_build::lv2::Lv2Unit {
    use truce_build::lv2::Lv2Unit;
    match s {
        "dB" | "Db" | "db" => Lv2Unit::Db,
        "Hz" | "hz" => Lv2Unit::Hz,
        "ms" => Lv2Unit::Milliseconds,
        "s" => Lv2Unit::Seconds,
        "%" => Lv2Unit::Percent,
        "st" => Lv2Unit::Semitones,
        "pan" => Lv2Unit::Pan,
        _ => Lv2Unit::None,
    }
}

fn parse_flags_value(s: &str) -> truce_build::lv2::Lv2Flags {
    let mut out = truce_build::lv2::Lv2Flags::default();
    for tok in s.split('|').map(str::trim) {
        match tok {
            "is_bypass" | "bypass" => out.is_bypass = true,
            "readonly" => out.readonly = true,
            "hidden" => out.hidden = true,
            _ => {}
        }
    }
    out
}

fn audio_io_for(c: truce_build::lv2::Lv2Category) -> (u32, u32) {
    use truce_build::lv2::Lv2Category;
    match c {
        Lv2Category::Instrument => (0, 2),
        Lv2Category::Effect | Lv2Category::Analyzer | Lv2Category::Tool => (2, 2),
        Lv2Category::NoteEffect => (0, 0),
    }
}

fn parse_category(s: &str) -> truce_build::lv2::Lv2Category {
    use truce_build::lv2::Lv2Category;
    // `truce.toml`'s `category = "midi"` resolves to
    // `PluginCategory::NoteEffect` at runtime; the sidecar TTL has to
    // agree with that mapping or the LV2 plugin ends up with the
    // wrong port set (missing `midi_out` for note effects, no MIDI
    // decode for instruments).
    match s.to_ascii_lowercase().as_str() {
        "instrument" => Lv2Category::Instrument,
        "midi" | "noteeffect" | "note_effect" | "note-effect" => Lv2Category::NoteEffect,
        "analyzer" | "analyser" => Lv2Category::Analyzer,
        "tool" | "utility" => Lv2Category::Tool,
        _ => Lv2Category::Effect,
    }
}

fn param_kind_str(k: crate::ParamKind) -> &'static str {
    match k {
        crate::ParamKind::Float => "Float",
        crate::ParamKind::Bool => "Bool",
        crate::ParamKind::Int => "Int",
        crate::ParamKind::Enum => "Enum",
    }
}

fn type_last_segment(ty: &Type) -> Option<String> {
    if let Type::Path(syn::TypePath { path, .. }) = ty {
        return path.segments.last().map(|s| s.ident.to_string());
    }
    None
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Where to write a per-struct sidecar for the *current* compile.
fn sidecar_dir() -> Option<PathBuf> {
    let pkg_name = std::env::var("CARGO_PKG_NAME").ok()?;
    let truce_toml = truce_build::find_truce_toml().ok()?;
    sidecar_dir_for(&pkg_name, &truce_toml)
}

fn sidecar_dir_for(pkg_name: &str, truce_toml: &std::path::Path) -> Option<PathBuf> {
    let workspace_root = truce_toml.parent()?;
    let target_dir = truce_build::target_dir(workspace_root);
    Some(target_dir.join("lv2-meta").join(pkg_name))
}
