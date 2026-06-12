//! Proc macros for truce plugins.
//!
//! Two distinct macro families live here:
//!
//! - `plugin_info!()` reads `truce.toml` at compile time and expands
//!   to a `PluginInfo` struct literal. Uses toml + serde at
//!   proc-macro compile time.
//! - `#[derive(Params)]`, `#[derive(ParamEnum)]`, `#[derive(State)]`
//!   generate the parameter-discovery / state-roundtrip glue every
//!   plugin needs. Pure syn + quote.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use std::collections::HashSet;
use syn::ext::IdentExt;
use syn::{Data, DeriveInput, Expr, Fields, Lit, Type, TypePath, UnOp};
use truce_build::{Config, PluginDef};
use truce_params::METER_ID_BASE;

mod lv2_emit;

/// Resolve `truce.toml` and pull out the `[[plugin]]` entry for the
/// current crate. Routes every failure mode through `Result<…, String>`
/// so callers can convert errors into `compile_error!` tokens with a
/// span - `panic!`-ing from a proc macro produces a span-less,
/// multi-line error frame instead of the clean compiler diagnostic the
/// caller actually wants.
pub(crate) fn try_resolve_plugin() -> Result<(Config, String, std::path::PathBuf), String> {
    let path = truce_build::find_truce_toml()?;
    let config = truce_build::load_config(&path)?;

    let pkg_name =
        std::env::var("CARGO_PKG_NAME").map_err(|_| "CARGO_PKG_NAME not set".to_string())?;
    if !config.plugin.iter().any(|p| p.crate_name == pkg_name) {
        let available: Vec<_> = config
            .plugin
            .iter()
            .map(|p| p.crate_name.as_str())
            .collect();
        return Err(format!(
            "No [[plugin]] entry with crate = \"{pkg_name}\" in {}. \
             Available: {}",
            path.display(),
            available.join(", ")
        ));
    }
    // Canonicalize so the embedded `include_bytes!` reference is stable
    // across `cargo check` / `cargo build` invocations (both resolve
    // CARGO_MANIFEST_DIR identically, but a future caller might run the
    // proc-macro from a symlinked path - canonicalizing pins the literal
    // to the realpath).
    let canonical = path.canonicalize().unwrap_or(path);
    Ok((config, pkg_name, canonical))
}

fn find_plugin<'a>(config: &'a Config, pkg_name: &str) -> &'a PluginDef {
    // try_resolve_plugin already verified the entry exists.
    config
        .plugin
        .iter()
        .find(|p| p.crate_name == pkg_name)
        .expect("try_resolve_plugin verified this entry exists")
}

/// Generate a `PluginInfo` struct literal from `truce.toml`.
///
/// Reads the `[[plugin]]` entry matching the current crate's package name
/// and the `[vendor]` section. No build.rs needed.
///
/// ```ignore
/// fn info() -> PluginInfo {
///     truce::plugin_info!()
/// }
/// ```
// `au_name` / `au3_name` (and similar `vst2_name` / `vst3_name`)
// mirror the user-facing truce.toml keys; renaming would break the
// 1:1 with the TOML schema.
#[allow(clippy::similar_names)]
#[proc_macro]
pub fn plugin_info(_input: TokenStream) -> TokenStream {
    let (config, pkg_name, truce_toml_path) = match try_resolve_plugin() {
        Ok(v) => v,
        Err(msg) => {
            // Surface the problem as a compile_error with a span at
            // the macro call-site instead of an opaque proc-macro
            // `panic!` (which renders as `error: proc macro panicked`
            // followed by a multi-line backtrace and no source span).
            return syn::Error::new(proc_macro2::Span::call_site(), msg)
                .to_compile_error()
                .into();
        }
    };
    let plugin = find_plugin(&config, &pkg_name);

    let name = &plugin.name;
    let bundle_id = &plugin.bundle_id;
    let vendor = &config.vendor.name;
    let url = &config.vendor.url;
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let version = plugin
        .version
        .as_deref()
        .unwrap_or(&pkg_version)
        .to_string();

    // Category-string vocabulary parsed by every consumer that reads
    // `truce.toml`. Note-effect plugins (`midi` / `note_effect`) must
    // map to a distinct variant from `Effect` so the LV2 MIDI input
    // decode path stays open; collapsing them silently drops every
    // host MIDI event.
    let category = match plugin.category.as_str() {
        "instrument" => quote! { ::truce::core::PluginCategory::Instrument },
        "midi" | "note_effect" => quote! { ::truce::core::PluginCategory::NoteEffect },
        "analyzer" => quote! { ::truce::core::PluginCategory::Analyzer },
        "tool" => quote! { ::truce::core::PluginCategory::Tool },
        _ => quote! { ::truce::core::PluginCategory::Effect },
    };
    // NoteEffect plugins map to `aumi` (Apple's MIDI Processor type).
    // Pairs with empty `bus_layouts` at the plugin level: aumi
    // plugins must not expose audio I/O. Logic routes `aumi` to the
    // MIDI FX slot, which is where arpeggiators / transposers /
    // note-shapers belong. A mismatch with the AU-type computed at
    // install / package time causes auval to report "Class Data
    // fields ... do not match component description".
    let au_type = plugin
        .au_type
        .as_deref()
        .unwrap_or(match plugin.category.as_str() {
            "instrument" => "aumu",
            "midi" | "note_effect" => "aumi",
            _ => "aufx",
        });

    let plugin_id = truce_build::plugin_id(&config.vendor.id, &plugin.name);

    let Some(resolved_fourcc) = plugin.fourcc.as_ref().or(plugin.au_subtype.as_ref()) else {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "truce.toml: [[plugin]] entry `{}` requires `fourcc` or `au_subtype`",
                plugin.crate_name
            ),
        )
        .to_compile_error()
        .into();
    };
    let au_manufacturer = &config.vendor.au_manufacturer;

    let aax_category = if let Some(cat) = &plugin.aax_category {
        quote! { Some(#cat) }
    } else {
        quote! { None }
    };
    let vst3_subcategory = if let Some(sub) = &plugin.vst3_subcategory {
        quote! { Some(#sub) }
    } else {
        quote! { None }
    };

    // Per-format display-name overrides. Empty strings are normalized
    // to `None` here so format wrappers don't need to repeat the
    // empty-vs-unset distinction at every call site.
    let opt_str = |s: &Option<String>| -> proc_macro2::TokenStream {
        match s.as_deref() {
            Some(v) if !v.is_empty() => quote! { Some(#v) },
            _ => quote! { None },
        }
    };
    let preset_user_dir = opt_str(&plugin.presets.as_ref().and_then(|c| c.user_dir.clone()));
    let vst3_name = opt_str(&plugin.vst3_name);
    let clap_name = opt_str(&plugin.clap_name);
    let vst2_name = opt_str(&plugin.vst2_name);
    let au_name = opt_str(&plugin.au_name);
    let au3_name = opt_str(&plugin.au3_name);
    let aax_name = opt_str(&plugin.aax_name);
    let lv2_name = opt_str(&plugin.lv2_name);
    let mute_preview_output = plugin.mute_preview_output;
    let min_subblock_samples = config.automation.min_subblock_samples;

    // `include_bytes!` registers `truce.toml` as a build-time dependency
    // through the compiler's normal dep-info tracking. Without it, edits
    // to truce.toml don't trigger a rebuild - proc macros on stable Rust
    // have no other way to declare external file dependencies.
    // Path is canonicalized in `try_resolve_plugin` so the literal is
    // stable across invocations.
    let truce_toml_lit = truce_toml_path.to_string_lossy().into_owned();
    let expanded = quote! {
        {
            const _TRUCE_TOML_DEP: &[u8] = include_bytes!(#truce_toml_lit);
            ::truce::core::PluginInfo {
                name: #name,
                vendor: #vendor,
                url: #url,
                version: #version,
                category: #category,
                bundle_id: #bundle_id,
                vst3_id: #plugin_id,
                clap_id: #plugin_id,
                fourcc: ::truce::core::info::fourcc(#resolved_fourcc.as_bytes()),
                au_type: ::truce::core::info::fourcc(#au_type.as_bytes()),
                au_manufacturer: ::truce::core::info::fourcc(#au_manufacturer.as_bytes()),
                aax_id: None,
                aax_category: #aax_category,
                vst3_subcategory: #vst3_subcategory,
                preset_user_dir: #preset_user_dir,
                vst3_name: #vst3_name,
                clap_name: #clap_name,
                vst2_name: #vst2_name,
                au_name: #au_name,
                au3_name: #au3_name,
                aax_name: #aax_name,
                lv2_name: #lv2_name,
                mute_preview_output: #mute_preview_output,
                automation: ::truce::core::info::AutomationConfig {
                    min_subblock_samples: #min_subblock_samples,
                },
            }
        }
    };

    expanded.into()
}

/// Emit `manifest.ttl` + `plugin.ttl` for the plugin whose root params
/// type is `<input>`. Invoked by `truce::plugin!`'s expansion. See
/// [`lv2_emit::emit_root_impl`] for the gory details.
///
/// Doc-hidden because plugin authors never call it directly - it's
/// part of the `truce::plugin!` machinery.
#[doc(hidden)]
#[proc_macro]
pub fn __truce_lv2_emit_root(input: TokenStream) -> TokenStream {
    lv2_emit::emit_root_impl(input)
}

/// Recognized parameter field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamKind {
    Float,
    Bool,
    Int,
    Enum,
}

/// A parsed parameter field from the input struct.
pub(crate) struct ParamField {
    pub(crate) ident: syn::Ident,
    pub(crate) kind: ParamKind,
    pub(crate) attrs: ParamAttrs,
    /// For `EnumParam<T>`, the inner type `T`.
    enum_type: Option<syn::Type>,
}

impl ParamField {
    /// ID that the auto-assignment block at the top of `derive_params`
    /// has guaranteed is populated. Calling this before the auto-assign
    /// loop runs is a logic error - the `expect` message names the
    /// invariant rather than just panicking with `unwrap`'s opaque
    /// `called Option::unwrap on a None`.
    pub(crate) fn id(&self) -> u32 {
        self.attrs
            .id
            .expect("ParamField::id called before the auto-assignment block ran")
    }
}

/// A nested Params field (delegates to inner struct).
pub(crate) struct NestedField {
    pub(crate) ident: syn::Ident,
    /// Field type, retained so the derive can call associated
    /// functions on it without an instance - specifically
    /// `Params::param_infos_static` for the registration-time
    /// "no temp plugin" path.
    pub(crate) ty: syn::Type,
}

/// A meter slot field.
pub(crate) struct MeterField {
    ident: syn::Ident,
    pub(crate) id: Option<u32>,
}

impl MeterField {
    /// ID that the auto-assignment block at the top of `derive_params`
    /// has guaranteed is populated. Same invariant as
    /// [`ParamField::id`].
    fn id(&self) -> u32 {
        self.id
            .expect("MeterField::id called before the auto-assignment block ran")
    }
}

/// A resolved `default = ...` expression plus the tokens to emit
/// for it. `value` drives the derive's compile-time range / shape
/// checks; `path_tokens` carries the original `std::f64::consts::*`
/// path through to the generated `ParamInfo` so clippy lints like
/// `approx_constant` see the path, not the resolved literal. For
/// numeric literals `path_tokens` stays `None` and the f64 value
/// gets quoted in the usual way.
#[derive(Clone)]
pub(crate) struct DefaultExpr {
    pub(crate) value: f64,
    pub(crate) path_tokens: Option<TokenStream2>,
}

/// Parsed `#[param(...)]` attributes.
#[derive(Default)]
pub(crate) struct ParamAttrs {
    pub(crate) id: Option<u32>,
    pub(crate) name: Option<String>,
    short_name: Option<String>,
    group: Option<String>,
    pub(crate) range: Option<String>,
    pub(crate) default: Option<DefaultExpr>,
    pub(crate) unit: Option<String>,
    pub(crate) flags: Option<String>,
    /// Set by `#[param(chunk = false)]` on parameters too expensive
    /// to retarget mid-block (FFT sizes, oversampling factors,
    /// lookahead spans). `None` means "use the default" (`true`).
    /// Drives the `ParamFlags::CHUNKED` bit in
    /// `gen_param_info_literal`. See the
    /// `parameter-dependent-chunking.md` design doc.
    pub(crate) chunk: Option<bool>,
    smooth: Option<String>,
    format_fn: Option<String>,
    parse_fn: Option<String>,
    /// Compile-error tokens collected during parsing - emitted by
    /// the derive output so unknown keys and unexpected literal
    /// kinds surface at compile time instead of as silent default
    /// values.
    errors: Vec<proc_macro2::TokenStream>,
}

fn type_last_segment(ty: &Type) -> Option<String> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    path.segments.last().map(|seg| seg.ident.to_string())
}

/// Extract the generic type argument from `EnumParam<T>`.
fn extract_enum_type_arg(ty: &Type) -> Option<syn::Type> {
    if let Type::Path(TypePath { path, .. }) = ty {
        let seg = path.segments.last()?;
        if seg.ident == "EnumParam"
            && let syn::PathArguments::AngleBracketed(args) = &seg.arguments
            && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
        {
            return Some(inner.clone());
        }
    }
    None
}

fn classify_param_type(ty: &Type) -> Option<ParamKind> {
    let name = type_last_segment(ty)?;
    match name.as_str() {
        "FloatParam" => Some(ParamKind::Float),
        "BoolParam" => Some(ParamKind::Bool),
        "IntParam" => Some(ParamKind::Int),
        "EnumParam" => Some(ParamKind::Enum),
        _ => None,
    }
}

/// Parse `#[param(...)]` attributes from a field. Errors carried in
/// `attrs.errors` instead of bubbling out so the caller can keep
/// collecting (each malformed attribute should produce a separate
/// `compile_error!` rather than the first one short-circuiting).
fn parse_param_attrs(field: &syn::Field) -> ParamAttrs {
    let mut attrs = ParamAttrs::default();
    // Helper: turn a `syn::Error` into a `compile_error!` token stream
    // and stash it. Used both by the explicit "unknown key" / "wrong
    // literal kind" arms and by `parse_nested_meta`'s own bubbled
    // errors below.
    let push_err = |attrs: &mut ParamAttrs, e: syn::Error| {
        attrs.errors.push(e.to_compile_error());
    };
    for attr in &field.attrs {
        if !attr.path().is_ident("param") {
            continue;
        }
        // `parse_nested_meta`'s closure can only return one error per
        // call (it short-circuits the *current* attribute group on
        // first Err), so route per-key errors through `attrs.errors`
        // instead - each malformed key generates a `compile_error!`
        // and parsing continues.
        let parse_result = attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            // Two-step pattern for the string-typed keys: parse the
            // literal first, then either assign or stash a
            // compile_error. Avoids needing `&mut attrs` aliased with
            // `&mut attrs.<field>` inside a closure.
            let take_str_into = |slot: &mut Option<String>,
                                 errors: &mut Vec<proc_macro2::TokenStream>,
                                 key_name: &str|
             -> syn::Result<()> {
                let value: Lit = meta.value()?.parse()?;
                match value {
                    Lit::Str(lit) => {
                        *slot = Some(lit.value());
                    }
                    other => {
                        errors.push(
                            syn::Error::new_spanned(
                                other,
                                format!("`#[param({key_name} = ...)]` expects a string literal"),
                            )
                            .to_compile_error(),
                        );
                    }
                }
                Ok(())
            };
            match key.as_str() {
                "id" => {
                    let value: Lit = meta.value()?.parse()?;
                    match value {
                        Lit::Int(lit) => attrs.id = Some(lit.base10_parse()?),
                        other => push_err(
                            &mut attrs,
                            syn::Error::new_spanned(
                                other,
                                "`#[param(id = ...)]` expects an integer literal",
                            ),
                        ),
                    }
                }
                "name" => take_str_into(&mut attrs.name, &mut attrs.errors, "name")?,
                "short_name" => {
                    take_str_into(&mut attrs.short_name, &mut attrs.errors, "short_name")?;
                }
                "group" => take_str_into(&mut attrs.group, &mut attrs.errors, "group")?,
                "range" => take_str_into(&mut attrs.range, &mut attrs.errors, "range")?,
                "default" => {
                    // `meta.value()` returns the stream after `=`. Parse as
                    // an `Expr` so we accept negative literals like
                    // `default = -1` (which `Lit` alone refuses - `-1` is
                    // an `Expr::Unary(Neg, Lit::Int(1))`, not a literal).
                    let expr: Expr = meta.value()?.parse()?;
                    match parse_default_expr(&expr) {
                        Some(value) => {
                            // Preserve the original token stream when the
                            // user wrote a const path (e.g.
                            // `std::f64::consts::SQRT_2`) so clippy
                            // lints against the macro expansion see the
                            // path, not the resolved literal. Numeric
                            // literals keep the existing f64-formatting
                            // pathway.
                            let path_tokens =
                                matches!(expr, Expr::Path(syn::ExprPath { qself: None, .. }))
                                    .then(|| expr.to_token_stream());
                            attrs.default = Some(DefaultExpr { value, path_tokens });
                        }
                        None => push_err(
                            &mut attrs,
                            syn::Error::new_spanned(
                                &expr,
                                "expected a numeric literal or `std::f64::consts::*` constant for \
                                 `default` (e.g. `default = 0.5`, `default = -1`, \
                                 `default = std::f64::consts::SQRT_2`)",
                            ),
                        ),
                    }
                }
                "unit" => take_str_into(&mut attrs.unit, &mut attrs.errors, "unit")?,
                "flags" => take_str_into(&mut attrs.flags, &mut attrs.errors, "flags")?,
                "smooth" => take_str_into(&mut attrs.smooth, &mut attrs.errors, "smooth")?,
                "format" => take_str_into(&mut attrs.format_fn, &mut attrs.errors, "format")?,
                "parse" => take_str_into(&mut attrs.parse_fn, &mut attrs.errors, "parse")?,
                "chunk" => {
                    let value: Lit = meta.value()?.parse()?;
                    match value {
                        Lit::Bool(lit) => attrs.chunk = Some(lit.value),
                        other => push_err(
                            &mut attrs,
                            syn::Error::new_spanned(
                                other,
                                "`#[param(chunk = ...)]` expects a bool literal (e.g. \
                                 `chunk = false` for params too expensive to retarget mid-block)",
                            ),
                        ),
                    }
                }
                other => {
                    push_err(
                        &mut attrs,
                        meta.error(format!(
                            "unknown `#[param]` key `{other}` (expected one of: id, name, \
                             short_name, group, range, default, unit, flags, smooth, format, \
                             parse, chunk)",
                        )),
                    );
                }
            }
            Ok(())
        });
        // `parse_nested_meta` itself can fail at the tokenizer level
        // (mis-typed `=`, stray punctuation). Surface those too.
        if let Err(e) = parse_result {
            push_err(&mut attrs, e);
        }
    }
    attrs
}

/// Check if a field has `#[nested]` attribute.
fn has_nested_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident("nested"))
}

/// Check if a field has `#[meter]` attribute.
fn has_meter_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident("meter"))
}

/// Check if a field type is `MeterSlot`.
fn is_meter_slot(ty: &Type) -> bool {
    type_last_segment(ty).is_some_and(|s| s == "MeterSlot")
}

/// Coerce a `default = ...` attribute expression into an `f64`.
///
/// Accepts numeric literals (positive and `-`-prefixed) and a
/// whitelisted set of `std::f64::consts::*` paths (also
/// `core::f64::consts::*` and bare `f64::consts::*`). Range and
/// shape checks downstream want a concrete `f64`, so anything
/// outside this shape returns `None` and the caller emits a
/// `compile_error!`.
//
// `i64 as f64` is an at-compile-time literal default whose magnitude
// is bounded by IntParam ranges; large enough to lose mantissa bits
// but well-defined for the validation round-trip.
#[allow(clippy::cast_precision_loss)]
fn parse_default_expr(expr: &Expr) -> Option<f64> {
    match expr {
        Expr::Lit(syn::ExprLit { lit, .. }) => match lit {
            Lit::Float(lit) => lit.base10_parse::<f64>().ok(),
            Lit::Int(lit) => lit.base10_parse::<i64>().ok().map(|n| n as f64),
            // `default = true` / `default = false` for BoolParam map to
            // exactly 1.0 / 0.0; `BoolParam::new` panics on anything
            // else, so this is the only path that produces a valid
            // bool default.
            Lit::Bool(lit) => Some(if lit.value { 1.0 } else { 0.0 }),
            _ => None,
        },
        Expr::Unary(syn::ExprUnary {
            op: UnOp::Neg(_),
            expr: inner,
            ..
        }) => parse_default_expr(inner).map(|v| -v),
        Expr::Path(syn::ExprPath {
            path, qself: None, ..
        }) => std_f64_const(path),
        _ => None,
    }
}

/// Resolve a `[std::|core::]?f64::consts::NAME` path to its `f64`
/// value. Returns `None` for any other shape so the caller can keep
/// using the existing `compile_error!` for unsupported expressions.
///
/// Closed whitelist so the derive's downstream range / shape checks
/// keep working with a concrete `f64` literal embedded in the
/// expansion.
fn std_f64_const(path: &syn::Path) -> Option<f64> {
    let segs: Vec<String> = path
        .segments
        .iter()
        .filter(|s| s.arguments.is_none())
        .map(|s| s.ident.to_string())
        .collect();
    if segs.len() != path.segments.len() {
        return None;
    }
    let prefix_ok = match segs.as_slice() {
        [a, b, _] => a == "f64" && b == "consts",
        [a, b, c, _] => (a == "std" || a == "core") && b == "f64" && c == "consts",
        _ => false,
    };
    if !prefix_ok {
        return None;
    }
    match segs.last()?.as_str() {
        "PI" => Some(std::f64::consts::PI),
        "TAU" => Some(std::f64::consts::TAU),
        "E" => Some(std::f64::consts::E),
        "SQRT_2" => Some(std::f64::consts::SQRT_2),
        "FRAC_1_SQRT_2" => Some(std::f64::consts::FRAC_1_SQRT_2),
        "FRAC_PI_2" => Some(std::f64::consts::FRAC_PI_2),
        "FRAC_PI_3" => Some(std::f64::consts::FRAC_PI_3),
        "FRAC_PI_4" => Some(std::f64::consts::FRAC_PI_4),
        "FRAC_PI_6" => Some(std::f64::consts::FRAC_PI_6),
        "FRAC_PI_8" => Some(std::f64::consts::FRAC_PI_8),
        "FRAC_1_PI" => Some(std::f64::consts::FRAC_1_PI),
        "FRAC_2_PI" => Some(std::f64::consts::FRAC_2_PI),
        "FRAC_2_SQRT_PI" => Some(std::f64::consts::FRAC_2_SQRT_PI),
        "LN_2" => Some(std::f64::consts::LN_2),
        "LN_10" => Some(std::f64::consts::LN_10),
        "LOG2_E" => Some(std::f64::consts::LOG2_E),
        "LOG10_E" => Some(std::f64::consts::LOG10_E),
        "LOG2_10" => Some(std::f64::consts::LOG2_10),
        "LOG10_2" => Some(std::f64::consts::LOG10_2),
        _ => None,
    }
}

/// Collect parameter fields, nested fields, and meter fields from a struct.
fn collect_fields(fields: &Fields) -> (Vec<ParamField>, Vec<NestedField>, Vec<MeterField>) {
    let Fields::Named(named) = fields else {
        return (Vec::new(), Vec::new(), Vec::new());
    };

    let mut params = Vec::new();
    let mut nested = Vec::new();
    let mut meters = Vec::new();

    for f in &named.named {
        let Some(ident) = f.ident.clone() else {
            continue;
        };

        if has_nested_attr(f) {
            nested.push(NestedField {
                ident,
                ty: f.ty.clone(),
            });
            continue;
        }

        if has_meter_attr(f) || is_meter_slot(&f.ty) {
            meters.push(MeterField { ident, id: None });
            continue;
        }

        if let Some(kind) = classify_param_type(&f.ty) {
            let attrs = parse_param_attrs(f);
            let enum_type = if kind == ParamKind::Enum {
                extract_enum_type_arg(&f.ty)
            } else {
                None
            };
            params.push(ParamField {
                ident,
                kind,
                attrs,
                enum_type,
            });
        }
    }

    (params, nested, meters)
}

/// Parse a range string like "linear(-60, 24)" into tokens.
fn parse_range_tokens(range: &str) -> proc_macro2::TokenStream {
    let bad = |msg: String| quote! { compile_error!(#msg) };

    if let Some(inner) = range
        .strip_prefix("linear(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 2 {
            return bad(format!(
                "linear range needs two arguments `linear(min, max)`, got `linear({inner})`"
            ));
        }
        let Ok(min) = parts[0].parse::<f64>() else {
            return bad(format!("linear range min `{}` is not a number", parts[0]));
        };
        let Ok(max) = parts[1].parse::<f64>() else {
            return bad(format!("linear range max `{}` is not a number", parts[1]));
        };
        if min >= max {
            return bad(format!(
                "linear range needs min < max, got `linear({min}, {max})`"
            ));
        }
        return quote! { ::truce::params::ParamRange::Linear { min: #min, max: #max } };
    }
    if let Some(inner) = range.strip_prefix("log(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 2 {
            return bad(format!(
                "log range needs two arguments `log(min, max)`, got `log({inner})`"
            ));
        }
        let Ok(min) = parts[0].parse::<f64>() else {
            return bad(format!("log range min `{}` is not a number", parts[0]));
        };
        let Ok(max) = parts[1].parse::<f64>() else {
            return bad(format!("log range max `{}` is not a number", parts[1]));
        };
        if min <= 0.0 || max <= 0.0 {
            return bad(format!(
                "log range needs strictly positive bounds, got `log({min}, {max})`"
            ));
        }
        if min >= max {
            return bad(format!(
                "log range needs min < max, got `log({min}, {max})`"
            ));
        }
        return quote! { ::truce::params::ParamRange::Logarithmic { min: #min, max: #max } };
    }
    if let Some(inner) = range
        .strip_prefix("discrete(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 2 {
            return bad(format!(
                "discrete range needs two arguments `discrete(min, max)`, got `discrete({inner})`"
            ));
        }
        let Ok(min) = parts[0].parse::<i64>() else {
            return bad(format!(
                "discrete range min `{}` is not an integer",
                parts[0]
            ));
        };
        let Ok(max) = parts[1].parse::<i64>() else {
            return bad(format!(
                "discrete range max `{}` is not an integer",
                parts[1]
            ));
        };
        if min >= max {
            return bad(format!(
                "discrete range needs min < max, got `discrete({min}, {max})`"
            ));
        }
        return quote! { ::truce::params::ParamRange::Discrete { min: #min, max: #max } };
    }
    if let Some(inner) = range
        .strip_prefix("enum(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let Ok(count) = inner.trim().parse::<usize>() else {
            return bad(format!(
                "enum count `{}` is not a non-negative integer",
                inner.trim()
            ));
        };
        if count < 2 {
            return bad(format!(
                "enum range needs at least 2 variants, got `enum({count})`"
            ));
        }
        return quote! { ::truce::params::ParamRange::Enum { count: #count } };
    }
    bad(format!(
        "unknown range `{range}` - supported: linear(min, max), log(min, max), discrete(min, max), enum(count)"
    ))
}

/// Parse a unit string into `ParamUnit` tokens.
fn parse_unit_tokens(unit: &str) -> proc_macro2::TokenStream {
    match unit {
        "dB" | "Db" | "db" => quote! { ::truce::params::ParamUnit::Db },
        "Hz" | "hz" => quote! { ::truce::params::ParamUnit::Hz },
        "ms" => quote! { ::truce::params::ParamUnit::Milliseconds },
        "s" => quote! { ::truce::params::ParamUnit::Seconds },
        "%" => quote! { ::truce::params::ParamUnit::Percent },
        "st" => quote! { ::truce::params::ParamUnit::Semitones },
        "pan" => quote! { ::truce::params::ParamUnit::Pan },
        "deg" | "°" => quote! { ::truce::params::ParamUnit::Degrees },
        "" | "none" => quote! { ::truce::params::ParamUnit::None },
        // Loud compile-error rather than silent fallback - typos like
        // `"hz "` (trailing space) or `"DB"` (uppercase) shouldn't map
        // to `ParamUnit::None` and surface only as "0.5" instead of
        // "0.5 Hz" in the host.
        other => {
            let msg =
                format!("unknown unit `{other}` - supported: dB, Hz, ms, s, %, st, pan, deg, none");
            quote! { compile_error!(#msg) }
        }
    }
}

/// Parse a flags string into `ParamFlags` tokens.
fn parse_flags_tokens(flags: &str) -> proc_macro2::TokenStream {
    let mut parts = Vec::new();
    for flag in flags.split('|').map(|s| s.trim().to_lowercase()) {
        match flag.as_str() {
            "automatable" => parts.push(quote! { ::truce::params::ParamFlags::AUTOMATABLE }),
            "hidden" => parts.push(quote! { ::truce::params::ParamFlags::HIDDEN }),
            "readonly" => parts.push(quote! { ::truce::params::ParamFlags::READONLY }),
            "bypass" => parts.push(quote! { ::truce::params::ParamFlags::IS_BYPASS }),
            _ => {}
        }
    }
    if parts.is_empty() {
        quote! { ::truce::params::ParamFlags::AUTOMATABLE }
    } else {
        quote! { #(#parts)|* }
    }
}

/// Parse a smoothing string into `SmoothingStyle` tokens. Same
/// loud-on-malformed contract as `parse_unit_tokens` /
/// `parse_range_tokens`: every typo emits a `compile_error!` instead
/// of silently swallowing the bad value.
fn parse_smooth_tokens(smooth: &str) -> proc_macro2::TokenStream {
    let bad = |msg: String| quote! { compile_error!(#msg) };
    if smooth == "none" {
        return quote! { ::truce::params::SmoothingStyle::None };
    }
    if let Some(inner) = smooth
        .strip_prefix("linear(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return match inner.trim().parse::<f64>() {
            Ok(ms) => quote! { ::truce::params::SmoothingStyle::Linear(#ms) },
            Err(_) => bad(format!(
                "smooth = \"linear({inner})\" expects a numeric milliseconds value \
                 (e.g. `smooth = \"linear(20)\"`)",
            )),
        };
    }
    if let Some(inner) = smooth
        .strip_prefix("exp(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return match inner.trim().parse::<f64>() {
            Ok(ms) => quote! { ::truce::params::SmoothingStyle::Exponential(#ms) },
            Err(_) => bad(format!(
                "smooth = \"exp({inner})\" expects a numeric milliseconds value \
                 (e.g. `smooth = \"exp(5)\"`)",
            )),
        };
    }
    bad(format!(
        "unknown smoothing style `{smooth}` - supported: \"none\", \"linear(<ms>)\", \"exp(<ms>)\"",
    ))
}

/// Build the `ParamInfo { ... }` literal for a `#[param(...)]` field.
///
/// Shared between [`gen_field_constructor`] (which wraps it in a
/// `FloatParam`/`BoolParam`/etc. constructor at runtime) and the
/// derive's static-metadata path
/// ([`Params::param_infos_static`](truce_params::Params::param_infos_static)),
/// which lifts the same literal into a `LazyLock<Vec<ParamInfo>>` so
/// format wrappers can read parameter metadata without constructing a
/// plugin instance. Returns `None` when a compile-time validation
/// (default-out-of-range etc.) failed; the caller's `compile_error!`
/// path handles that branch.
fn gen_param_info_literal(f: &ParamField) -> Option<proc_macro2::TokenStream> {
    let a = &f.attrs;
    let id = f.id();
    let name = a.name.as_deref().unwrap_or("Unnamed");
    let short_name = a.short_name.as_deref().unwrap_or(name);
    let group = a.group.as_deref().unwrap_or("");
    let default_plain: TokenStream2 = match a.default.as_ref() {
        Some(DefaultExpr {
            path_tokens: Some(t),
            ..
        }) => t.clone(),
        Some(DefaultExpr { value, .. }) => {
            let v = *value;
            quote! { #v }
        }
        None => quote! { 0.0 },
    };

    if let Some(d) = a.default.as_ref().map(|d| d.value) {
        // Integer round-trip exactness checks - an epsilon-based
        // comparison would silently accept fractional defaults like
        // `2.5` for an `Int` / `Enum` param. The `as i64` / `as u32`
        // truncations are the round-trip's whole point.
        #[allow(
            clippy::float_cmp,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let invalid = match f.kind {
            ParamKind::Bool => d != 0.0 && d != 1.0,
            ParamKind::Int => !d.is_finite() || (d as i64 as f64) != d,
            ParamKind::Enum => !d.is_finite() || d < 0.0 || f64::from(d as u32) != d,
            ParamKind::Float => !d.is_finite(),
        };
        if invalid {
            return None;
        }
    }

    let range = match &a.range {
        Some(r) => parse_range_tokens(r),
        None => match f.kind {
            ParamKind::Bool => quote! { ::truce::params::ParamRange::Discrete { min: 0, max: 1 } },
            ParamKind::Enum => {
                if let Some(ref enum_ty) = f.enum_type {
                    quote! { ::truce::params::ParamRange::Enum { count: <#enum_ty as ::truce::params::ParamEnum>::variant_count() } }
                } else {
                    quote! { ::truce::params::ParamRange::Enum { count: 2 } }
                }
            }
            _ => quote! { ::truce::params::ParamRange::Linear { min: 0.0, max: 1.0 } },
        },
    };

    let unit = if let Some(u) = &a.unit {
        parse_unit_tokens(u)
    } else {
        quote! { ::truce::params::ParamUnit::None }
    };

    // The explicit-flags path lets a plugin pass `flags = "hidden |
    // bypass"` to override AUTOMATABLE; OR in CHUNKED on the default
    // path (and on the explicit path unless the plugin opted out via
    // `chunk = false`) so the wrapper-side chunker treats every param
    // as a split point by default. See the
    // `parameter-dependent-chunking.md` design doc.
    let base_flags = if let Some(fl) = &a.flags {
        parse_flags_tokens(fl)
    } else {
        quote! { ::truce::params::ParamFlags::AUTOMATABLE }
    };
    let flags = if a.chunk.unwrap_or(true) {
        quote! { (#base_flags).union(::truce::params::ParamFlags::CHUNKED) }
    } else {
        base_flags
    };

    let kind = match f.kind {
        ParamKind::Float => quote! { ::truce::params::ParamValueKind::Float },
        ParamKind::Int => quote! { ::truce::params::ParamValueKind::Int },
        ParamKind::Bool => quote! { ::truce::params::ParamValueKind::Bool },
        ParamKind::Enum => quote! { ::truce::params::ParamValueKind::Enum },
    };

    Some(quote! {
        ::truce::params::ParamInfo {
            id: #id,
            name: #name,
            short_name: #short_name,
            group: #group,
            range: #range,
            default_plain: #default_plain,
            flags: #flags,
            unit: #unit,
            kind: #kind,
        }
    })
}

/// Generate a constructor call for a field with `#[param(...)]` attributes.
///
/// `f.id()` carries the `expect`-guarded "must run after auto-assign"
/// invariant; using it here surfaces the order-of-call contract at
/// construction time instead of silently minting `id = 0`. Today every
/// caller drives this via `assign_param_ids` first, but a future
/// refactor that calls `gen_field_constructor` out of order panics
/// with a precise message rather than producing colliding id=0 params.
fn gen_field_constructor(f: &ParamField) -> proc_macro2::TokenStream {
    let a = &f.attrs;
    let name = a.name.as_deref().unwrap_or("Unnamed");

    // Compile-time sanity check on `default = ...`. Surfaces user
    // errors that would otherwise silently saturate at runtime (`as
    // u32` on a negative `default_plain`, `as i64` on a fractional
    // value). The variant-count range check for `EnumParam` still
    // runs at construction time because `variant_count()` isn't
    // visible to the macro at expansion time without per-call
    // const-eval plumbing.
    if let Some(d) = a.default.as_ref().map(|d| d.value) {
        // Integer round-trip exactness checks - an epsilon-based
        // comparison would silently accept fractional defaults like
        // `2.5` for an `Int` / `Enum` param. The `as i64` / `as u32`
        // truncations are the round-trip's whole point.
        #[allow(
            clippy::float_cmp,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let err = match f.kind {
            ParamKind::Bool if d != 0.0 && d != 1.0 => {
                Some(format!("BoolParam default {name} must be 0 or 1; got {d}"))
            }
            ParamKind::Int if !d.is_finite() || (d as i64 as f64) != d => Some(format!(
                "IntParam '{name}' default must be an integer literal; got {d}"
            )),
            ParamKind::Enum if !d.is_finite() || d < 0.0 || f64::from(d as u32) != d => {
                Some(format!(
                    "EnumParam '{name}' default must be a non-negative integer (variant index); got {d}"
                ))
            }
            ParamKind::Float if !d.is_finite() => Some(format!(
                "FloatParam '{name}' default must be finite; got {d}"
            )),
            _ => None,
        };
        if let Some(msg) = err {
            return quote! { compile_error!(#msg); };
        }
    }

    let Some(info) = gen_param_info_literal(f) else {
        // Validation block above already returned a `compile_error!`
        // for every shape that `gen_param_info_literal` rejects.
        // Surface a fallback diagnostic so a future divergence
        // between the two checks fails loudly instead of silently
        // emitting bad code.
        let msg = format!("invalid `#[param]` attributes on field `{name}`");
        return quote! { compile_error!(#msg); };
    };

    match f.kind {
        ParamKind::Float => {
            let smooth = if let Some(s) = &a.smooth {
                parse_smooth_tokens(s)
            } else {
                quote! { ::truce::params::SmoothingStyle::None }
            };
            quote! { ::truce::params::FloatParam::new(#info, #smooth) }
        }
        ParamKind::Bool => quote! { ::truce::params::BoolParam::new(#info) },
        ParamKind::Int => quote! { ::truce::params::IntParam::new(#info) },
        ParamKind::Enum => quote! { ::truce::params::EnumParam::new(#info) },
    }
}

// ============================================================================
// Main derive macro
// ============================================================================

/// # Panics
///
/// Panics if `syn` fails to parse the input token stream. That only
/// happens on syntactically broken input (rustc would already be
/// rejecting the same file), so the panic surfaces a derive-internal
/// regression rather than user error.
#[proc_macro_derive(Params, attributes(param, nested, meter))]
#[allow(clippy::too_many_lines)]
pub fn derive_params(input: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("Failed to parse input for Params derive");
    let struct_name = &ast.ident;

    let fields = match &ast.data {
        Data::Struct(data) => &data.fields,
        _ => {
            return syn::Error::new_spanned(&ast, "Params can only be derived on structs")
                .to_compile_error()
                .into();
        }
    };

    let (mut param_fields, nested_fields, mut meter_fields) = collect_fields(fields);

    if param_fields.is_empty() && nested_fields.is_empty() && meter_fields.is_empty() {
        return syn::Error::new_spanned(
            &ast,
            "Params derive: no recognized fields (FloatParam, BoolParam, IntParam, EnumParam, MeterSlot, or #[nested])",
        )
        .to_compile_error()
        .into();
    }

    // --- Auto-assign parameter IDs ---
    // Explicit IDs take priority. Auto-assigned IDs fill gaps starting at 0.
    {
        let explicit_ids: HashSet<u32> = param_fields.iter().filter_map(|f| f.attrs.id).collect();
        let mut next_auto = 0u32;
        for f in &mut param_fields {
            if f.attrs.id.is_none() {
                while explicit_ids.contains(&next_auto) {
                    next_auto += 1;
                }
                f.attrs.id = Some(next_auto);
                next_auto += 1;
            }
        }
    }

    // --- Auto-assign meter IDs ---
    // Meters live in a dedicated high-range starting at 2^24 so they
    // can never collide with auto-assigned param IDs (which fill from
    // 0 upward). Storage indexes as `meter_array[id - METER_ID_BASE]`.
    // `METER_ID_BASE` is imported from `truce_params` at proc-macro
    // build time so the value can't drift between crates.
    for (next_meter, m) in (METER_ID_BASE..).zip(meter_fields.iter_mut()) {
        m.id = Some(next_meter);
    }

    // --- Compile-time validation: duplicate IDs + range overlap ---
    //
    // Checks:
    //  1. No two params share an ID.
    //  2. No explicit param ID lands in the meter range (≥ METER_ID_BASE).
    //     Auto-assigned params can't hit this - you'd need 16M fields.
    //  3. No param ID collides with any meter ID (follows from #2 when
    //     both checks pass, but surfaced separately for a clearer error).
    {
        let mut seen_ids = HashSet::new();
        for f in &param_fields {
            if let Some(id) = f.attrs.id {
                if id >= METER_ID_BASE {
                    let msg = format!(
                        "Parameter ID {id} is in the meter range (≥ {METER_ID_BASE}). \
                         Param IDs must be < {METER_ID_BASE}."
                    );
                    return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
                }
                if !seen_ids.insert(id) {
                    let msg = format!("Duplicate parameter ID: {id}");
                    return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
                }
            }
        }
        for m in &meter_fields {
            if let Some(id) = m.id
                && !seen_ids.insert(id)
            {
                let msg = format!("Meter ID {id} collides with a parameter ID.");
                return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
            }
        }
    }

    // --- Compile-time LV2 metadata sidecar ---
    //
    // Each Params struct (root or nested, plugin crate or helper)
    // writes `<target>/lv2-meta/<crate>/<struct>.params.toml` with its
    // own params, meters, and #[nested] child type names. The final
    // TTL render happens later via `__truce_lv2_emit_root!`, which
    // `truce::plugin!` invokes with the root params type and which
    // walks the sidecar tree to aggregate. Failures here are silent -
    // they surface at TTL-emit time when the aggregator can't find
    // the data it needs.
    let nested_for_sidecar: Vec<(syn::Ident, syn::Type)> = nested_fields
        .iter()
        .map(|n| (n.ident.clone(), n.ty.clone()))
        .collect();
    lv2_emit::write_struct_sidecar(
        struct_name,
        &param_fields,
        &meter_fields,
        &nested_for_sidecar,
    );

    // --- Always generate new() ---
    let generate_new = !param_fields.is_empty() || !meter_fields.is_empty();

    // --- Count ---
    let own_count = param_fields.len();
    let nested_idents: Vec<_> = nested_fields.iter().map(|n| &n.ident).collect();
    let count_expr = if nested_fields.is_empty() {
        quote! { #own_count }
    } else {
        quote! { #own_count #(+ self.#nested_idents.count())* }
    };

    // --- param_infos ---
    let own_infos: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = &f.ident;
            quote! { self.#ident.info.clone() }
        })
        .collect();

    let infos_expr = if nested_fields.is_empty() {
        quote! { vec![#(#own_infos),*] }
    } else {
        // Recurse via `append_param_infos` so each nested struct
        // pushes directly into the shared buffer instead of building
        // its own `Vec` that the outer call then extends. Saves
        // O(depth) intermediate allocations per `param_infos()` call.
        quote! {
            let mut infos = vec![#(#own_infos),*];
            #(self.#nested_idents.append_param_infos(&mut infos);)*
            infos
        }
    };

    // Override `append_param_infos` so the buffer-based form
    // recurses without the extra `Vec` round-trip in nested cases.
    // Plain (non-nested) structs accept the default impl.
    let append_infos_impl = if nested_fields.is_empty() {
        quote! {}
    } else {
        quote! {
            fn append_param_infos(&self, into: &mut Vec<::truce::params::ParamInfo>) {
                #(into.push(#own_infos);)*
                #(self.#nested_idents.append_param_infos(into);)*
            }
        }
    };

    // --- param_infos_static ---
    // Same shape as `param_infos`, but each entry is the raw
    // `ParamInfo { ... }` literal (built by
    // `gen_param_info_literal`) rather than a runtime `self.<f>.info`
    // read. Lifted into a `LazyLock<Vec<ParamInfo>>` so format
    // wrappers' `register_*` paths can read parameter metadata
    // without constructing a plugin instance. AAX's `Describe` runs
    // at C++ static-init time and can't safely allocate a plugin
    // there, so the static path is mandatory for that format.
    let own_info_literals: Vec<proc_macro2::TokenStream> = param_fields
        .iter()
        .filter_map(gen_param_info_literal)
        .collect();
    let nested_static_calls: Vec<proc_macro2::TokenStream> = nested_fields
        .iter()
        .map(|n| {
            let ty = &n.ty;
            quote! {
                infos.extend(<#ty as ::truce::params::Params>::param_infos_static());
            }
        })
        .collect();
    let static_infos_body = if nested_fields.is_empty() {
        quote! { vec![#(#own_info_literals),*] }
    } else {
        quote! {
            {
                let mut infos: Vec<::truce::params::ParamInfo> = vec![#(#own_info_literals),*];
                #(#nested_static_calls)*
                infos
            }
        }
    };
    let param_infos_static_impl = quote! {
        fn param_infos_static() -> Vec<::truce::params::ParamInfo>
        where
            Self: ::std::marker::Sized,
        {
            // `LazyLock` so the first call computes the metadata and
            // every later registration reads the cache. `clone()` is
            // a single Vec allocation - cheap relative to the avoided
            // plugin construction. (`ParamInfo` is `Clone`.)
            static INFOS: ::std::sync::LazyLock<Vec<::truce::params::ParamInfo>> =
                ::std::sync::LazyLock::new(|| #static_infos_body);
            INFOS.clone()
        }
    };

    // --- meter_ids ---
    let own_meter_ids: Vec<_> = meter_fields
        .iter()
        .map(|m| {
            let ident = &m.ident;
            quote! { self.#ident.id() }
        })
        .collect();
    let meter_ids_expr = if nested_fields.is_empty() {
        quote! { vec![#(#own_meter_ids),*] }
    } else {
        quote! {
            let mut ids = vec![#(#own_meter_ids),*];
            #(ids.extend(self.#nested_idents.meter_ids());)*
            ids
        }
    };

    // --- get_plain ---
    let get_plain_arms: Vec<_> = param_fields.iter().map(|f| {
        let ident = &f.ident;
        match f.kind {
            ParamKind::Float => quote! { x if x == self.#ident.id() => Some(self.#ident.raw_target()), },
            // `i64 as f64` is precision-lossy by spec (mantissa 53 < 63);
            // no `From<i64> for f64` exists, so the cast is the idiom.
            ParamKind::Int => quote! { x if x == self.#ident.id() => {
                #[allow(clippy::cast_precision_loss)]
                let v = self.#ident.value() as f64;
                Some(v)
            }, },
            ParamKind::Bool => quote! { x if x == self.#ident.id() => Some(if self.#ident.value() { 1.0 } else { 0.0 }), },
            // `u32 → f64` is lossless (u32::MAX < 2^53); use `From` for
            // consistency with the rest of the derive output.
            ParamKind::Enum => quote! { x if x == self.#ident.id() => Some(f64::from(self.#ident.index())), },
        }
    }).collect();

    let get_plain_fallthrough = if nested_fields.is_empty() {
        quote! { _ => None, }
    } else {
        quote! {
            _ => {
                #(if let Some(v) = self.#nested_idents.get_plain(id) { return Some(v); })*
                None
            }
        }
    };

    // --- get_normalized ---
    //
    // Per-id match arms reach into the matching param's `info.range`
    // and call `normalize` / `denormalize` directly. Dispatching
    // through `self.param_infos()` would allocate a `Vec<ParamInfo>`
    // on every host-driven `set_normalized` / `get_normalized` round
    // trip and every `EditorBridge` paint frame.
    let get_normalized_arms: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let plain_expr = match f.kind {
                ParamKind::Float => quote! { self.#ident.raw_target() },
                // i64 → f64 has no `From`; `as` with an explicit
                // allow is the idiom.
                ParamKind::Int => quote! {{
                    #[allow(clippy::cast_precision_loss)]
                    let v = self.#ident.value() as f64;
                    v
                }},
                ParamKind::Bool => quote! { if self.#ident.value() { 1.0 } else { 0.0 } },
                // u32 → f64 is lossless: use `From`.
                ParamKind::Enum => quote! { f64::from(self.#ident.index()) },
            };
            quote! {
                x if x == self.#ident.id() => Some(self.#ident.info.range.normalize(#plain_expr)),
            }
        })
        .collect();

    let get_normalized_fallthrough = if nested_fields.is_empty() {
        quote! { _ => None, }
    } else {
        quote! {
            _ => {
                #(if let Some(v) = self.#nested_idents.get_normalized(id) { return Some(v); })*
                None
            }
        }
    };

    // --- set_plain ---
    let set_plain_arms: Vec<_> = param_fields.iter().map(|f| {
        let ident = &f.ident;
        match f.kind {
            ParamKind::Float => quote! { x if x == self.#ident.id() => self.#ident.set_value(value), },
            ParamKind::Bool => quote! { x if x == self.#ident.id() => self.#ident.set_value(value > 0.5), },
            ParamKind::Int => quote! { x if x == self.#ident.id() => self.#ident.set_value(value.round() as i64), },
            ParamKind::Enum => quote! { x if x == self.#ident.id() => self.#ident.set_index(value.round() as u32), },
        }
    }).collect();

    let set_plain_fallthrough = if nested_fields.is_empty() {
        quote! { _ => {} }
    } else {
        quote! {
            _ => {
                #(self.#nested_idents.set_plain(id, value);)*
            }
        }
    };

    // --- set_normalized ---
    //
    // Per-id arms denormalize through the matching param's range, then
    // commit through the kind-specific atomic write. Same allocation
    // motivation as `get_normalized` above.
    let set_normalized_arms: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let commit = match f.kind {
                ParamKind::Float => quote! { self.#ident.set_value(plain) },
                ParamKind::Bool => quote! { self.#ident.set_value(plain > 0.5) },
                ParamKind::Int => quote! { self.#ident.set_value(plain.round() as i64) },
                ParamKind::Enum => quote! { self.#ident.set_index(plain.round() as u32) },
            };
            quote! {
                x if x == self.#ident.id() => {
                    let plain = self.#ident.info.range.denormalize(value);
                    #commit;
                }
            }
        })
        .collect();

    let set_normalized_fallthrough = if nested_fields.is_empty() {
        quote! { _ => {} }
    } else {
        quote! {
            _ => {
                #(self.#nested_idents.set_normalized(id, value);)*
            }
        }
    };

    // --- format_value ---
    let format_value_arms: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = &f.ident;
            if let Some(ref fmt_fn) = f.attrs.format_fn {
                let fmt_ident = syn::Ident::new(fmt_fn, ident.span());
                quote! { x if x == self.#ident.id() => Some(self.#fmt_ident(value)), }
            } else {
                match f.kind {
                    ParamKind::Bool => quote! {
                        x if x == self.#ident.id() => {
                            Some(if value > 0.5 { "On".to_string() } else { "Off".to_string() })
                        }
                    },
                    ParamKind::Enum => {
                        let enum_ty = f
                            .enum_type
                            .as_ref()
                            .expect("ParamKind::Enum field must have enum_type populated");
                        quote! {
                            x if x == self.#ident.id() => {
                                Some(::truce::params::EnumParam::<#enum_ty>::format_by_index(value))
                            }
                        }
                    }
                    _ => quote! {
                        x if x == self.#ident.id() => {
                            Some(::truce::params::format_param_value(&self.#ident.info, value))
                        }
                    },
                }
            }
        })
        .collect();

    let format_fallthrough = if nested_fields.is_empty() {
        quote! { _ => None, }
    } else {
        quote! {
            _ => {
                #(if let Some(v) = self.#nested_idents.format_value(id, value) { return Some(v); })*
                None
            }
        }
    };

    // --- parse_value ---
    let parse_value_arms: Vec<_> = param_fields
        .iter()
        .filter_map(|f| {
            let parse_fn = f.attrs.parse_fn.as_ref()?;
            let ident = &f.ident;
            let parse_ident = syn::Ident::new(parse_fn, ident.span());
            Some(quote! { x if x == self.#ident.id() => self.#parse_ident(text), })
        })
        .collect();

    let parse_value_impl = if parse_value_arms.is_empty() && nested_fields.is_empty() {
        quote! { None }
    } else {
        let nested_parse = if nested_fields.is_empty() {
            quote! { _ => None, }
        } else {
            quote! {
                _ => {
                    #(if let Some(v) = self.#nested_idents.parse_value(id, text) { return Some(v); })*
                    None
                }
            }
        };
        quote! {
            match id {
                #(#parse_value_arms)*
                #nested_parse
            }
        }
    };

    // --- snap_smoothers ---
    let snap_stmts: Vec<_> = param_fields
        .iter()
        .filter(|f| f.kind == ParamKind::Float)
        .map(|f| {
            let ident = &f.ident;
            quote! { self.#ident.smoother.snap(self.#ident.raw_target()); }
        })
        .collect();

    // --- set_sample_rate ---
    let sr_stmts: Vec<_> = param_fields
        .iter()
        .filter(|f| f.kind == ParamKind::Float)
        .map(|f| {
            let ident = &f.ident;
            quote! { self.#ident.smoother.set_sample_rate(sample_rate); }
        })
        .collect();

    // --- collect_values ---
    let collect_ids: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = &f.ident;
            quote! { self.#ident.id() }
        })
        .collect();

    // --- Generate new() ---
    let new_impl = if generate_new {
        let param_inits: Vec<_> = param_fields
            .iter()
            .map(|f| {
                let ident = &f.ident;
                let constructor = gen_field_constructor(f);
                quote! { #ident: #constructor }
            })
            .collect();

        let meter_inits: Vec<_> = meter_fields
            .iter()
            .map(|m| {
                let ident = &m.ident;
                let id = m.id();
                quote! { #ident: ::truce::params::MeterSlot { id: #id } }
            })
            .collect();

        quote! {
            impl #struct_name {
                pub fn new() -> Self {
                    let me = Self {
                        #(#param_inits,)*
                        #(#meter_inits,)*
                    };
                    // The compile-time ID-collision check only sees
                    // the IDs declared in *this* struct; a parent ID
                    // matching a nested-struct ID compiles cleanly
                    // and would silently corrupt state round-trip.
                    // Surface the bug as a panic at construction
                    // instead.
                    <Self as ::truce::params::Params>::assert_no_id_collisions(&me);
                    me
                }
            }

            impl Default for #struct_name {
                fn default() -> Self {
                    Self::new()
                }
            }
        }
    } else {
        quote! {}
    };

    // --- Generate ParamId enum (includes both params and meters) ---
    let param_id_enum = if !param_fields.is_empty() || !meter_fields.is_empty() {
        let enum_name = syn::Ident::new(&format!("{struct_name}ParamId"), struct_name.span());

        let param_variants: Vec<_> = param_fields
            .iter()
            .map(|f| {
                let variant = snake_to_pascal(&f.ident);
                let id_lit = proc_macro2::Literal::u32_unsuffixed(f.id());
                quote! { #variant = #id_lit }
            })
            .collect();

        let meter_variants: Vec<_> = meter_fields
            .iter()
            .map(|m| {
                let variant = snake_to_pascal(&m.ident);
                let id_lit = proc_macro2::Literal::u32_unsuffixed(m.id());
                quote! { #variant = #id_lit }
            })
            .collect();

        let variants: Vec<_> = param_variants
            .iter()
            .chain(meter_variants.iter())
            .cloned()
            .collect();

        let from_u32_arms: Vec<_> = param_fields
            .iter()
            .map(|f| {
                let variant = snake_to_pascal(&f.ident);
                let id_lit = proc_macro2::Literal::u32_unsuffixed(f.id());
                quote! { #id_lit => Some(#enum_name::#variant) }
            })
            .chain(meter_fields.iter().map(|m| {
                let variant = snake_to_pascal(&m.ident);
                let id_lit = proc_macro2::Literal::u32_unsuffixed(m.id());
                quote! { #id_lit => Some(#enum_name::#variant) }
            }))
            .collect();

        quote! {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            #[repr(u32)]
            pub enum #enum_name {
                #(#variants,)*
            }

            impl #enum_name {
                pub const fn as_u32(self) -> u32 {
                    self as u32
                }

                pub fn from_u32(id: u32) -> Option<Self> {
                    match id {
                        #(#from_u32_arms,)*
                        _ => None,
                    }
                }
            }

            impl From<#enum_name> for u32 {
                fn from(id: #enum_name) -> u32 {
                    id as u32
                }
            }
        }
    } else {
        quote! {}
    };

    // Surface every `compile_error!` collected by `parse_param_attrs`
    // (unknown keys, wrong literal kinds, malformed `default = ...`).
    // Emitted alongside the impl rather than instead of it so the
    // diagnostics are precise; downstream type errors from a malformed
    // attribute aren't masked by a missing `Params` impl.
    let attr_errors: Vec<proc_macro2::TokenStream> = param_fields
        .iter()
        .flat_map(|f| f.attrs.errors.iter().cloned())
        .collect();

    let expanded = quote! {
        #(#attr_errors)*

        #new_impl

        #param_id_enum

        impl ::truce::params::__private::Sealed for #struct_name {}

        impl ::truce::params::Params for #struct_name {
            fn param_infos(&self) -> Vec<::truce::params::ParamInfo> {
                #infos_expr
            }

            #append_infos_impl

            #param_infos_static_impl

            fn count(&self) -> usize {
                #count_expr
            }

            fn meter_ids(&self) -> Vec<u32> {
                #meter_ids_expr
            }

            fn get_normalized(&self, id: u32) -> Option<f64> {
                match id {
                    #(#get_normalized_arms)*
                    #get_normalized_fallthrough
                }
            }

            fn set_normalized(&self, id: u32, value: f64) {
                match id {
                    #(#set_normalized_arms)*
                    #set_normalized_fallthrough
                }
            }

            fn get_plain(&self, id: u32) -> Option<f64> {
                match id {
                    #(#get_plain_arms)*
                    #get_plain_fallthrough
                }
            }

            fn set_plain(&self, id: u32, value: f64) {
                match id {
                    #(#set_plain_arms)*
                    #set_plain_fallthrough
                }
            }

            fn format_value(&self, id: u32, value: f64) -> Option<String> {
                match id {
                    #(#format_value_arms)*
                    #format_fallthrough
                }
            }

            fn parse_value(&self, id: u32, text: &str) -> Option<f64> {
                #parse_value_impl
            }

            fn snap_smoothers(&self) {
                #(#snap_stmts)*
                #(self.#nested_idents.snap_smoothers();)*
            }

            fn set_sample_rate(&self, sample_rate: f64) {
                #(#sr_stmts)*
                #(self.#nested_idents.set_sample_rate(sample_rate);)*
            }

            fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
                let mut ids: Vec<u32> = vec![#(#collect_ids),*];
                let mut values: Vec<f64> = ids
                    .iter()
                    .map(|id| self.get_plain(*id).expect("id was emitted by #[derive(Params)] and so must resolve"))
                    .collect();
                #({
                    let (nids, nvals) = self.#nested_idents.collect_values();
                    ids.extend(nids);
                    values.extend(nvals);
                })*
                (ids, values)
            }

            fn restore_values(&self, values: &[(u32, f64)]) {
                for (id, value) in values {
                    self.set_plain(*id, *value);
                }
            }
        }
    };

    expanded.into()
}

/// Convert a `snake_case` field name to a `PascalCase` enum variant ident.
///
/// Handles the awkward edge cases the original `split('_').map(...)` form
/// would silently produce nonsense for:
///
/// - **Raw idents** (`r#type`): the `r#` prefix is stripped via
///   `Ident::unraw()` rather than string-prefix matching, so callers
///   don't need to know whether `syn` rendered the ident raw.
/// - **Leading digit after underscore strip** (`_3band` → `"3band"`):
///   `split('_')` drops the leading `_`, leaving a fragment that can't
///   start an enum variant. The guard prepends `_` to produce
///   `"_3band"`. (Pure `r#3band` is unreachable - `Ident::new_raw`
///   rejects digit-first idents - but `_3band` is a legal Rust ident
///   and is the case the test actually exercises.)
/// - **All-non-alphanumeric** (`__`, `_`): `Ident::new("", span)` would
///   panic, so we fall back to `_` as a single-char placeholder. The
///   user already wrote a degenerate field name; the variant is still a
///   valid (if ugly) ident, surfacing the cause in compile errors.
fn snake_to_pascal(ident: &syn::Ident) -> syn::Ident {
    let raw = ident.unraw().to_string();
    let mut pascal: String = raw
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if pascal.is_empty() {
        pascal.push('_');
    } else if pascal.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        pascal.insert(0, '_');
    }
    syn::Ident::new(&pascal, ident.span())
}

// ============================================================================
// ParamEnum derive macro
// ============================================================================

/// Derive `ParamEnum` for a C-like enum.
///
/// Generates `Clone`, `Copy`, `PartialEq`, `Eq`, and all 5 `ParamEnum`
/// methods: `from_index`, `to_index`, `name`, `variant_count`, and
/// `variant_names`.
///
/// Display names default to the variant identifier. Use `#[name = "..."]`
/// on a variant to override:
///
/// ```ignore
/// #[derive(ParamEnum)]
/// pub enum ArpPattern {
///     Up,
///     Down,
///     #[name = "Up/Down"]
///     UpDown,
///     Random,
/// }
/// ```
/// # Panics
///
/// Panics if `syn` fails to parse the input token stream - same
/// "rustc-already-rejected" condition as [`derive_params`].
#[proc_macro_derive(ParamEnum, attributes(name))]
#[allow(clippy::too_many_lines)]
pub fn derive_param_enum(input: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("Failed to parse input for ParamEnum derive");
    let enum_name = &ast.ident;

    let variants = match &ast.data {
        Data::Enum(data) => &data.variants,
        _ => {
            return syn::Error::new_spanned(&ast, "ParamEnum can only be derived on enums")
                .to_compile_error()
                .into();
        }
    };

    // Ensure all variants are unit variants (no fields)
    for v in variants {
        if !matches!(v.fields, Fields::Unit) {
            return syn::Error::new_spanned(
                v,
                "ParamEnum variants must be unit variants (no fields)",
            )
            .to_compile_error()
            .into();
        }
    }

    let count = variants.len();

    let variant_idents: Vec<_> = variants.iter().map(|v| &v.ident).collect();

    // Parse #[name = "..."] attributes, falling back to the variant ident
    let variant_names: Vec<String> = variants
        .iter()
        .map(|v| {
            for attr in &v.attrs {
                if attr.path().is_ident("name")
                    && let Ok(syn::MetaNameValue {
                        value:
                            syn::Expr::Lit(syn::ExprLit {
                                lit: Lit::Str(lit), ..
                            }),
                        ..
                    }) = attr.meta.require_name_value()
                {
                    return lit.value();
                }
            }
            v.ident.to_string()
        })
        .collect();

    // from_index match arms
    let from_index_arms: Vec<_> = variant_idents
        .iter()
        .enumerate()
        .map(|(i, ident)| {
            quote! { #i => Self::#ident, }
        })
        .collect();
    let first_variant = &variant_idents[0];

    // to_index match arms
    let to_index_arms: Vec<_> = variant_idents
        .iter()
        .enumerate()
        .map(|(i, ident)| {
            quote! { Self::#ident => #i, }
        })
        .collect();

    // name match arms
    let name_arms: Vec<_> = variant_idents
        .iter()
        .zip(variant_names.iter())
        .map(|(ident, name)| {
            quote! { Self::#ident => #name, }
        })
        .collect();

    let name_strs: Vec<_> = variant_names
        .iter()
        .map(std::string::String::as_str)
        .collect();

    let expanded = quote! {
        #[allow(clippy::expl_impl_clone_on_copy)]
        impl Clone for #enum_name {
            fn clone(&self) -> Self { *self }
        }
        impl Copy for #enum_name {}
        impl PartialEq for #enum_name {
            fn eq(&self, other: &Self) -> bool {
                ::truce::params::ParamEnum::to_index(self) == ::truce::params::ParamEnum::to_index(other)
            }
        }
        impl Eq for #enum_name {}

        impl ::truce::params::__private::Sealed for #enum_name {}

        impl ::truce::params::ParamEnum for #enum_name {
            fn from_index(index: usize) -> Self {
                match index {
                    #(#from_index_arms)*
                    _ => Self::#first_variant,
                }
            }

            fn to_index(&self) -> usize {
                match self {
                    #(#to_index_arms)*
                }
            }

            fn name(&self) -> &'static str {
                match self {
                    #(#name_arms)*
                }
            }

            fn variant_count() -> usize {
                #count
            }

            fn variant_names() -> &'static [&'static str] {
                &[#(#name_strs),*]
            }
        }
    };

    expanded.into()
}

// ---------------------------------------------------------------------------
// #[derive(State)] - binary serialization for custom plugin state
// ---------------------------------------------------------------------------

/// Derive binary serialization for a custom state struct.
///
/// The struct must also implement `Default`. Missing fields during
/// deserialization are filled with defaults (forward compatibility).
///
/// Supported field types: `u8`, `u16`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`,
/// `f32`, `f64`, `bool`, `String`, `Vec<T>`, `Option<T>`, and nested `State` types.
///
/// ```ignore
/// #[derive(State, Default)]
/// pub struct MyState {
///     pub instance_name: String,
///     pub view_mode: u8,
///     pub selected_ids: Vec<u32>,
/// }
/// ```
/// # Panics
///
/// Panics if `syn` fails to parse the input token stream - same
/// "rustc-already-rejected" condition as [`derive_params`].
#[proc_macro_derive(State)]
pub fn derive_state(input: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("Failed to parse input for State derive");
    let name = &ast.ident;

    let fields = match &ast.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return syn::Error::new_spanned(
                    &ast,
                    "State can only be derived on structs with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(&ast, "State can only be derived on structs")
                .to_compile_error()
                .into();
        }
    };

    // `fields` is the syn-parsed field list of a single struct; can't
    // overflow u32. truce-derive is a proc-macro crate so it can't
    // pull in `truce_core::cast::len_u32`.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "struct field count is bounded by syn parser limits"
    )]
    let field_count = fields.len() as u32;
    let field_idents: Vec<_> = fields
        .iter()
        .map(|f| {
            f.ident
                .as_ref()
                .expect("Fields::Named guarantees every field carries an ident")
        })
        .collect();

    let write_fields: Vec<_> = field_idents
        .iter()
        .map(|ident| {
            quote! {
                {
                    let field_start = buf.len();
                    buf.extend_from_slice(&0u32.to_le_bytes());
                    ::truce::core::custom_state::StateField::write_field(&self.#ident, &mut buf);
                    let field_len = (buf.len() - field_start - 4) as u32;
                    buf[field_start..field_start + 4].copy_from_slice(&field_len.to_le_bytes());
                }
            }
        })
        .collect();

    let read_fields: Vec<_> = field_idents.iter().map(|ident| {
        quote! {
            if field_idx < stored_count {
                // `cursor.read_bytes(4)` returns a 4-byte slice when
                // `Some` (or `None` if the cursor is short). The
                // `try_into` to `[u8; 4]` therefore can't fail when
                // we're inside the `if let Some(_)` arm - but
                // routing through `.ok()` instead of `.unwrap()`
                // keeps the panic path closed for the case where a
                // future change to `read_bytes` relaxes its
                // length-guarantee. Matches the `stored_count` site
                // a few lines down.
                if let Some(len_bytes) = cursor.read_bytes(4)
                    && let Ok(len_arr) = <[u8; 4]>::try_from(len_bytes)
                {
                    let field_len = u32::from_le_bytes(len_arr) as usize;
                    let pos_before = cursor.remaining();
                    if let Some(val) = ::truce::core::custom_state::StateField::read_field(&mut cursor) {
                        result.#ident = val;
                    }
                    let consumed = pos_before - cursor.remaining();
                    if consumed < field_len {
                        let _ = cursor.read_bytes(field_len - consumed);
                    }
                }
                field_idx += 1;
            }
        }
    }).collect();

    let expanded = quote! {
        impl ::truce::core::custom_state::State for #name {
            fn serialize(&self) -> Vec<u8> {
                let mut buf = Vec::new();
                buf.extend_from_slice(&#field_count.to_le_bytes());
                #(#write_fields)*
                buf
            }

            fn deserialize(data: &[u8]) -> Option<Self> {
                let mut cursor = ::truce::core::custom_state::StateCursor::new(data);
                let count_bytes = cursor.read_bytes(4)?;
                // Cap `stored_count` at the cursor's remaining-byte
                // count: each forward-compat field carries at minimum
                // a 4-byte length prefix (read inside `skip_field`),
                // so a `stored_count` larger than the data can possibly
                // hold is hostile / corrupt input. The break inside
                // the loop already terminates on `skip_field()` →
                // false, but bounding `stored_count` up front keeps a
                // multi-GB synthetic count from forcing a long loop
                // before the buffer underrun is detected.
                let stored_count = (u32::from_le_bytes(count_bytes.try_into().ok()?) as usize)
                    .min(cursor.remaining() / 4 + #field_count as usize);
                let mut result = Self::default();
                let mut field_idx: usize = 0;
                #(#read_fields)*
                while field_idx < stored_count {
                    if !cursor.skip_field() { break; }
                    field_idx += 1;
                }
                Some(result)
            }
        }
    };

    expanded.into()
}

#[cfg(test)]
mod snake_to_pascal_tests {
    use super::snake_to_pascal;
    use proc_macro2::Span;

    fn convert(s: &str) -> String {
        // Keywords need raw-ident syntax to round-trip through `syn::Ident`.
        // Idents starting with a digit aren't constructible at all (not
        // even as `Ident::new_raw`); a leading-`_` ident like `_3band`
        // *is* valid and exercises the same branch after `split('_')`
        // strips the underscore.
        let id = if matches!(s, "type" | "fn" | "let" | "match") {
            syn::Ident::new_raw(s, Span::call_site())
        } else {
            syn::Ident::new(s, Span::call_site())
        };
        snake_to_pascal(&id).to_string()
    }

    #[test]
    fn ordinary_snake_case() {
        assert_eq!(convert("gain"), "Gain");
        assert_eq!(convert("low_pass"), "LowPass");
        assert_eq!(convert("multi_word_field"), "MultiWordField");
    }

    #[test]
    fn raw_keyword_ident() {
        assert_eq!(convert("type"), "Type");
    }

    #[test]
    fn leading_digit_prepends_underscore() {
        // `_3band` is a legal Rust ident; `split('_')` strips the leading
        // underscore and the surviving fragment "3band" starts with a
        // digit, which can't begin an enum variant. Guard prepends `_` so
        // the output `_3band` is a valid variant ident.
        assert_eq!(convert("_3band"), "_3band");
    }

    #[test]
    fn all_underscores_falls_back_to_underscore() {
        // `___` would otherwise produce "" → Ident::new("") panic.
        assert_eq!(convert("___"), "_");
    }
}

#[cfg(test)]
mod parse_default_tests {
    use super::parse_default_expr;

    fn eval(src: &str) -> Option<f64> {
        let expr: syn::Expr = syn::parse_str(src).expect("test input must parse as Expr");
        parse_default_expr(&expr)
    }

    #[test]
    fn numeric_literals() {
        assert_eq!(eval("0.5"), Some(0.5));
        assert_eq!(eval("3"), Some(3.0));
        assert_eq!(eval("-1"), Some(-1.0));
        assert_eq!(eval("-0.25"), Some(-0.25));
        assert_eq!(eval("true"), Some(1.0));
        assert_eq!(eval("false"), Some(0.0));
    }

    #[test]
    fn std_f64_const_paths() {
        // All three accepted prefixes resolve to the same value.
        let pi = std::f64::consts::PI;
        assert_eq!(eval("std::f64::consts::PI"), Some(pi));
        assert_eq!(eval("core::f64::consts::PI"), Some(pi));
        assert_eq!(eval("f64::consts::PI"), Some(pi));
        // Negation composes with `Expr::Unary`.
        assert_eq!(eval("-std::f64::consts::PI"), Some(-pi));
        // A handful of others to confirm the table.
        assert_eq!(eval("std::f64::consts::TAU"), Some(std::f64::consts::TAU));
        assert_eq!(
            eval("std::f64::consts::SQRT_2"),
            Some(std::f64::consts::SQRT_2)
        );
        assert_eq!(
            eval("std::f64::consts::FRAC_PI_2"),
            Some(std::f64::consts::FRAC_PI_2)
        );
        assert_eq!(eval("std::f64::consts::LN_2"), Some(std::f64::consts::LN_2));
    }

    #[test]
    fn rejected_shapes() {
        // Unknown const ident under an accepted prefix.
        assert_eq!(eval("std::f64::consts::DOES_NOT_EXIST"), None);
        // Bare ident: ambiguous with crate-local consts, so not accepted.
        assert_eq!(eval("PI"), None);
        // Arbitrary crate path.
        assert_eq!(eval("crate::FOO"), None);
        // `f32::consts::*` is out of scope - the macro embeds `f64`.
        assert_eq!(eval("std::f32::consts::PI"), None);
        // Function calls and arithmetic expressions are not const-evaluated.
        assert_eq!(eval("some_fn()"), None);
        assert_eq!(eval("1 + 2"), None);
    }
}
