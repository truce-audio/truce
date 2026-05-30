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
    nested: &[(syn::Ident, Type)],
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
        let name = p.attrs.name.clone().unwrap_or_else(|| p.ident.to_string());
        let _ = writeln!(buf, "name = \"{}\"", toml_escape(&name));
        let _ = writeln!(buf, "kind = \"{}\"", param_kind_str(p.kind));
        if let Some(r) = &p.attrs.range {
            let _ = writeln!(buf, "range = \"{}\"", toml_escape(r));
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
    for (_field, ty) in nested {
        if let Some(t) = type_last_segment(ty) {
            buf.push_str("[[nested]]\n");
            let _ = writeln!(buf, "type = \"{t}\"\n");
        }
    }
    let _ = std::fs::write(out_dir.join(format!("{struct_name}.params.toml")), buf);
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
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Err(msg) = aggregate(
        &sidecar_dir,
        &root_struct,
        &mut params,
        &mut meter_ids,
        &mut visited,
    ) {
        let lit = msg;
        return quote! { compile_error!(#lit); }.into();
    }

    let category = parse_category(&plugin.category);
    let (audio_in, audio_out) = audio_io_for(category);
    let accepts_midi_in = matches!(
        category,
        truce_build::lv2::Lv2Category::Instrument | truce_build::lv2::Lv2Category::NoteEffect
    );
    let has_midi_out = matches!(category, truce_build::lv2::Lv2Category::NoteEffect);

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

    TokenStream::new()
}

/// Recursively walk `<root>.params.toml`, then each `[[nested]]`
/// reference, accumulating params + meter IDs into the supplied vecs.
fn aggregate(
    sidecar_dir: &std::path::Path,
    struct_name: &str,
    params: &mut Vec<truce_build::lv2::Lv2Param>,
    meter_ids: &mut Vec<u32>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    if !visited.insert(struct_name.to_string()) {
        // Cycle - Rust wouldn't let one compile, but defend anyway.
        return Ok(());
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

    if let Some(toml::Value::Array(arr)) = toml.get("param") {
        for entry in arr {
            let p = parse_param_entry(entry).map_err(|e| format!("{}: {e}", path.display()))?;
            params.push(p);
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
        for entry in arr {
            if let Some(t) = entry.get("type").and_then(|v| v.as_str()) {
                aggregate(sidecar_dir, t, params, meter_ids, visited)?;
            }
        }
    }
    Ok(())
}

fn parse_param_entry(v: &toml::Value) -> Result<Lv2Param, String> {
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
    let range = parse_range_value(range_str, kind)?;
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

fn parse_range_value(s: &str, kind: &str) -> Result<truce_build::lv2::Lv2Range, String> {
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
        "Enum" => Ok(Lv2Range::Enum { count: 0 }),
        _ => Err(format!("unrecognised range `{s}`")),
    }
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
