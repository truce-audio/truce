//! Compile-time emission of the format-neutral metadata tree
//! (`truce_build::meta`).
//!
//! Two cooperating proc-macros fill it, and neither renders anything:
//!
//! 1. `derive(Params)` / `derive(ParamEnum)` write one *sidecar* per
//!    type - `<Struct>.params.toml` with that struct's own params,
//!    meters and `#[nested]` child type names; `<Enum>.enum.toml` with
//!    a variant count and display names. Every invocation in every
//!    crate writes its own, including helper / utility / test structs;
//!    unreferenced ones are simply never read.
//!
//! 2. `__truce_meta_root!(<params_type>)`, invoked by `truce::plugin!`,
//!    writes `root.toml` naming the root params type - the entry point
//!    [`truce_build::meta::resolve_root`] starts its walk from.
//!
//! The sidecars are inert data: no TTL, no plist, no format vocabulary.
//! `cargo truce` reads them at *package* time, when it knows which
//! formats it is building. That split is the point. The render used to
//! happen here, at proc-macro expansion, which meant a plugin that
//! merely enabled the `lv2` feature paid for LV2's encoding limits as
//! `compile_error!`s, and no format could ever live outside this repo.
//!
//! Writes are best-effort - a failure here is silent and surfaces at
//! package time, where [`truce_build::meta::read_root`] can say what is
//! missing and why.
//!
//! Known limits:
//!
//! - Cross-crate `#[nested]` is unsupported. The walk looks for sidecars
//!   in the plugin's own `<target>/truce-meta/<crate>/` directory, so a
//!   nested `Params` type from another crate has none there.
//! - `Params` and `ParamEnum` type names must be unique within a crate.
//!   Sidecars are keyed by the bare type name, because a derive can't
//!   see its type's module path at expansion time - so `osc::Settings`
//!   and `filter::Settings` share one `Settings.params.toml`, the last
//!   derive to run wins, and both nested slots resolve to it. Nothing
//!   fails: the walk just produces the wrong ports and names, which
//!   for LV2 are persisted in host sessions. Rename one of the types.

use crate::{ArrayLen, MeterField, ParamField};
use proc_macro::TokenStream;
use std::fmt::Write as _;
use std::path::PathBuf;
use syn::Type;

/// One `#[nested]` field as the sidecar records it.
pub(crate) struct SidecarNested<'a> {
    pub(crate) field: syn::Ident,
    pub(crate) ty: Type,
    pub(crate) base: Option<u32>,
    pub(crate) array_len: Option<&'a ArrayLen>,
    /// Name prefix where the group's params need disambiguating.
    pub(crate) label: String,
}

/// Write `<struct_name>.params.toml`.
pub(crate) fn write_struct_sidecar(
    struct_name: &syn::Ident,
    hash_scheme: bool,
    params: &[ParamField],
    meters: &[MeterField],
    nested: &[SidecarNested],
) {
    let Some(out_dir) = meta_dir() else {
        return;
    };
    let mut buf = String::new();
    let _ = writeln!(buf, "struct = \"{struct_name}\"");
    // The id scheme drives how the walk bases nested groups: hash -> a
    // hash of the slot field name; ordinal -> packed counts.
    let _ = writeln!(
        buf,
        "scheme = \"{}\"\n",
        if hash_scheme { "hash" } else { "ordinal" }
    );
    for p in params {
        buf.push_str("[[param]]\n");
        let _ = writeln!(buf, "id = {}", p.id());
        // The Rust field identifier: the stable, bare-TOML-safe key
        // `.preset` files resolve param names through.
        let _ = writeln!(
            buf,
            "field = \"{}\"",
            truce_build::toml_escape(&p.ident.to_string())
        );
        let name = p.attrs.name.clone().unwrap_or_else(|| p.ident.to_string());
        let _ = writeln!(buf, "name = \"{}\"", truce_build::toml_escape(&name));
        let _ = writeln!(buf, "kind = \"{}\"", param_kind_str(p.kind));
        if let Some(r) = &p.attrs.range {
            let _ = writeln!(buf, "range = \"{}\"", truce_build::toml_escape(r));
            // A const bound has no value at expansion time. Record that
            // as a fact; whether it is fatal is the reading format's
            // call (LV2 can't encode one, so it refuses).
            if crate::range_has_const(r) {
                let _ = writeln!(buf, "range_const = true");
            }
        } else if matches!(p.kind, crate::ParamKind::Enum)
            && let Some(ty) = p.enum_type()
            && let Some(seg) = type_last_segment(ty)
        {
            // No explicit `enum(N)`: record the enum's bare type name so
            // the walk can resolve its variant count from the
            // `<Enum>.enum.toml` sidecar `derive(ParamEnum)` wrote.
            let _ = writeln!(buf, "enum_type = \"{}\"", truce_build::toml_escape(&seg));
        }
        // Always emit `default` so a rendered port matches the runtime
        // `ParamInfo::default_plain`. The runtime falls back to `0.0`
        // when `#[param(default = ...)]` is omitted (see
        // `gen_param_info_literal`); omitting it here would leave a
        // format reading the range's minimum instead (gain at -60 dB)
        // while VST3 / standalone honour 0.0. A const default has no
        // expansion-time value, so it's flagged like a const range.
        match p.attrs.default.as_ref() {
            Some(d) if d.value.is_none() => {
                let _ = writeln!(buf, "default_const = true");
            }
            other => {
                let default = other.and_then(|d| d.value).unwrap_or(0.0);
                let _ = writeln!(buf, "default = {default}");
            }
        }
        if let Some(u) = &p.attrs.unit {
            let _ = writeln!(buf, "unit = \"{}\"", truce_build::toml_escape(u));
        }
        if let Some(f) = &p.attrs.flags {
            let _ = writeln!(buf, "flags = \"{}\"", truce_build::toml_escape(f));
        }
        if let Some(k) = p.attrs.midi_map {
            let binding = match k {
                crate::MidiBindKind::Cc(n) => format!("cc({n})"),
                crate::MidiBindKind::PitchBend => "pitchbend".to_string(),
                crate::MidiBindKind::ChannelPressure => "pressure".to_string(),
                crate::MidiBindKind::ProgramChange => "program".to_string(),
            };
            let _ = writeln!(buf, "midi_binding = \"{binding}\"");
            if let Some(ch) = p.attrs.midi_channel {
                let _ = writeln!(buf, "midi_channel = {ch}");
            }
        }
        buf.push('\n');
    }
    for m in meters {
        if let Some(id) = m.id {
            buf.push_str("[[meter]]\n");
            let _ = writeln!(buf, "id = {id}\n");
        }
    }
    for SidecarNested {
        field,
        ty,
        base,
        array_len,
        label,
    } in nested
    {
        if let Some(t) = type_last_segment(ty) {
            buf.push_str("[[nested]]\n");
            let _ = writeln!(buf, "type = \"{t}\"");
            // The field identifier preserves declaration order and lets
            // the walk base and name auto-packed groups
            // deterministically.
            let _ = writeln!(
                buf,
                "field = \"{}\"",
                truce_build::toml_escape(&field.to_string())
            );
            let _ = writeln!(buf, "label = \"{}\"", truce_build::toml_escape(label));
            if let Some(b) = base {
                let _ = writeln!(buf, "base = {b}");
            }
            // An array group is walked `array_len` times, each element
            // placed by `mix_nested_id` (hash scheme) or packed after
            // the one before it (ordinal). A const length can't be
            // evaluated here, so it's recorded as written and the walk
            // refuses it with that name.
            match array_len {
                Some(ArrayLen::Literal(n)) => {
                    let _ = writeln!(buf, "array_len = {n}");
                }
                Some(len @ ArrayLen::Const(_)) => {
                    let _ = writeln!(
                        buf,
                        "array_len_const = \"{}\"",
                        truce_build::toml_escape(&len.display())
                    );
                }
                None => {}
            }
            buf.push('\n');
        }
    }
    write_whole(&out_dir.join(format!("{struct_name}.params.toml")), &buf);
}

/// Write `<enum_name>.enum.toml` - a `ParamEnum`'s variant count and
/// display names, in index order.
///
/// This derive is the only place the count is known at expansion time
/// (it counts the enum's arms); a params sidecar references the enum by
/// name and the walk joins the two. Without it, a range-less
/// `EnumParam<T>` resolved to `enum(0)`, which LV2 rendered as an
/// invalid `lv2:maximum 0` / `lv2:default 1` port that REAPER rejects.
/// The names become each value's label (LV2's `lv2:scalePoint`).
pub(crate) fn write_enum_sidecar(enum_name: &syn::Ident, variant_names: &[String]) {
    let Some(out_dir) = meta_dir() else {
        return;
    };
    let mut buf = format!("count = {}\n", variant_names.len());
    buf.push_str("names = [");
    for (i, name) in variant_names.iter().enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        let _ = write!(buf, "\"{}\"", truce_build::toml_escape(name));
    }
    buf.push_str("]\n");
    write_whole(&out_dir.join(format!("{enum_name}.enum.toml")), &buf);
}

/// Implementation of `__truce_meta_root!(<params_type>)`: record the
/// root params type in `root.toml` so a package-time reader knows where
/// to start the walk. Expands to nothing.
pub(crate) fn write_root(input: TokenStream) -> TokenStream {
    // The input is the explicit `params:` type. `syn::Path` is the right
    // shape ("the params type's name") because `syn::Type` rejects bare
    // paths when the surrounding macro re-tokenizes them.
    let path: syn::Path = match syn::parse(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error().into(),
    };
    let Some(seg) = path.segments.last() else {
        return quote::quote! { compile_error!("__truce_meta_root!: empty params path"); }.into();
    };
    if let (Some(out_dir), Ok(pkg_name)) = (meta_dir(), std::env::var("CARGO_PKG_NAME")) {
        write_whole(
            &out_dir.join("root.toml"),
            &truce_build::meta::render_root(&seg.ident.to_string(), &pkg_name),
        );
    }
    TokenStream::new()
}

/// `<target>/truce-meta/<crate>`, created and stamped with its schema
/// version. `None` when there is no `truce.toml` above this crate or
/// the directory can't be created - a sidecar write is best-effort.
fn meta_dir() -> Option<PathBuf> {
    let pkg_name = std::env::var("CARGO_PKG_NAME").ok()?;
    let truce_toml = truce_build::find_truce_toml().ok()?;
    let target_dir = truce_build::target_dir(truce_toml.parent()?);
    let dir = truce_build::meta::meta_dir(&target_dir, &pkg_name);
    std::fs::create_dir_all(&dir).ok()?;
    // Stamp the layout version alongside the data it describes, so a
    // reader can refuse a tree it doesn't understand instead of
    // misparsing one.
    write_whole(&dir.join("schema"), &truce_build::meta::SCHEMA.to_string());
    Some(dir)
}

/// Replace `path` with `contents` in one step: write a sibling temp file,
/// then rename it over the target. Two builds expand these derives into
/// the same directory - rust-analyzer's `cargo check` alongside a
/// `cargo truce` build, say - and a plain `fs::write` truncates in place,
/// so a reader could see a half-written sidecar. A rename is atomic on
/// one filesystem, so a reader sees the old file or the new one. The temp
/// name carries the process id so concurrent builds don't share it.
/// Best-effort like every write here: on failure the temp is removed.
fn write_whole(path: &std::path::Path, contents: &str) {
    let Some(name) = path.file_name() else {
        return;
    };
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    if std::fs::write(&tmp, contents).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
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
