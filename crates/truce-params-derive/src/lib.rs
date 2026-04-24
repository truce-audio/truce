#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use quote::quote;
use std::collections::HashSet;
use syn::{Data, DeriveInput, Fields, Lit, Type, TypePath};

/// Recognized parameter field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    Float,
    Bool,
    Int,
    Enum,
}

/// A parsed parameter field from the input struct.
struct ParamField {
    ident: syn::Ident,
    kind: ParamKind,
    attrs: ParamAttrs,
    /// For EnumParam<T>, the inner type T.
    enum_type: Option<syn::Type>,
}

/// A nested Params field (delegates to inner struct).
struct NestedField {
    ident: syn::Ident,
}

/// A meter slot field.
struct MeterField {
    ident: syn::Ident,
    id: Option<u32>,
}

/// Parsed `#[param(...)]` attributes.
#[derive(Default)]
struct ParamAttrs {
    id: Option<u32>,
    name: Option<String>,
    short_name: Option<String>,
    group: Option<String>,
    range: Option<String>,
    default: Option<f64>,
    unit: Option<String>,
    flags: Option<String>,
    smooth: Option<String>,
    format_fn: Option<String>,
    parse_fn: Option<String>,
}

fn type_last_segment(ty: &Type) -> Option<String> {
    if let Type::Path(TypePath { path, .. }) = ty {
        path.segments.last().map(|seg| seg.ident.to_string())
    } else {
        None
    }
}

/// Extract the generic type argument from `EnumParam<T>`.
fn extract_enum_type_arg(ty: &Type) -> Option<syn::Type> {
    if let Type::Path(TypePath { path, .. }) = ty {
        let seg = path.segments.last()?;
        if seg.ident == "EnumParam" {
            if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    return Some(inner.clone());
                }
            }
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

/// Parse `#[param(...)]` attributes from a field.
fn parse_param_attrs(field: &syn::Field) -> ParamAttrs {
    let mut attrs = ParamAttrs::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("param") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            match key.as_str() {
                "id" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Int(lit) = value {
                        attrs.id = Some(lit.base10_parse()?);
                    }
                }
                "name" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.name = Some(lit.value());
                    }
                }
                "short_name" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.short_name = Some(lit.value());
                    }
                }
                "group" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.group = Some(lit.value());
                    }
                }
                "range" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.range = Some(lit.value());
                    }
                }
                "default" => {
                    let value: Lit = meta.value()?.parse()?;
                    match value {
                        Lit::Float(lit) => attrs.default = Some(lit.base10_parse()?),
                        Lit::Int(lit) => attrs.default = Some(lit.base10_parse::<i64>()? as f64),
                        _ => {}
                    }
                }
                "unit" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.unit = Some(lit.value());
                    }
                }
                "flags" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.flags = Some(lit.value());
                    }
                }
                "smooth" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.smooth = Some(lit.value());
                    }
                }
                "format" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.format_fn = Some(lit.value());
                    }
                }
                "parse" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(lit) = value {
                        attrs.parse_fn = Some(lit.value());
                    }
                }
                _ => {}
            }
            Ok(())
        });
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
    type_last_segment(ty).map_or(false, |s| s == "MeterSlot")
}

/// Check if a field has `#[param(...)]` attribute.
#[allow(dead_code)]
fn has_param_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident("param"))
}

/// Collect parameter fields, nested fields, and meter fields from a struct.
fn collect_fields(fields: &Fields) -> (Vec<ParamField>, Vec<NestedField>, Vec<MeterField>) {
    let named = match fields {
        Fields::Named(named) => named,
        _ => return (Vec::new(), Vec::new(), Vec::new()),
    };

    let mut params = Vec::new();
    let mut nested = Vec::new();
    let mut meters = Vec::new();

    for f in &named.named {
        let ident = match f.ident.clone() {
            Some(i) => i,
            None => continue,
        };

        if has_nested_attr(f) {
            nested.push(NestedField { ident });
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
    if let Some(inner) = range
        .strip_prefix("linear(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if parts.len() == 2 {
            let min: f64 = parts[0].parse().unwrap_or(0.0);
            let max: f64 = parts[1].parse().unwrap_or(1.0);
            return quote! { ::truce::params::ParamRange::Linear { min: #min, max: #max } };
        }
    }
    if let Some(inner) = range.strip_prefix("log(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if parts.len() == 2 {
            let min: f64 = parts[0].parse().unwrap_or(20.0);
            let max: f64 = parts[1].parse().unwrap_or(20000.0);
            return quote! { ::truce::params::ParamRange::Logarithmic { min: #min, max: #max } };
        }
    }
    if let Some(inner) = range
        .strip_prefix("discrete(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if parts.len() == 2 {
            let min: i64 = parts[0].parse().unwrap_or(0);
            let max: i64 = parts[1].parse().unwrap_or(1);
            return quote! { ::truce::params::ParamRange::Discrete { min: #min, max: #max } };
        }
    }
    if let Some(inner) = range
        .strip_prefix("enum(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let count: usize = inner.trim().parse().unwrap_or(2);
        return quote! { ::truce::params::ParamRange::Enum { count: #count } };
    }
    // Default
    quote! { ::truce::params::ParamRange::Discrete { min: 0, max: 1 } }
}

/// Parse a unit string into ParamUnit tokens.
fn parse_unit_tokens(unit: &str) -> proc_macro2::TokenStream {
    match unit {
        "dB" | "Db" | "db" => quote! { ::truce::params::ParamUnit::Db },
        "Hz" | "hz" => quote! { ::truce::params::ParamUnit::Hz },
        "ms" => quote! { ::truce::params::ParamUnit::Milliseconds },
        "s" => quote! { ::truce::params::ParamUnit::Seconds },
        "%" => quote! { ::truce::params::ParamUnit::Percent },
        "st" => quote! { ::truce::params::ParamUnit::Semitones },
        "pan" => quote! { ::truce::params::ParamUnit::Pan },
        _ => quote! { ::truce::params::ParamUnit::None },
    }
}

/// Parse a flags string into ParamFlags tokens.
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

/// Parse a smoothing string into SmoothingStyle tokens.
fn parse_smooth_tokens(smooth: &str) -> proc_macro2::TokenStream {
    if smooth == "none" {
        return quote! { ::truce::params::SmoothingStyle::None };
    }
    if let Some(inner) = smooth
        .strip_prefix("linear(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let ms: f64 = inner.trim().parse().unwrap_or(20.0);
        return quote! { ::truce::params::SmoothingStyle::Linear(#ms) };
    }
    if let Some(inner) = smooth
        .strip_prefix("exp(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let ms: f64 = inner.trim().parse().unwrap_or(5.0);
        return quote! { ::truce::params::SmoothingStyle::Exponential(#ms) };
    }
    quote! { ::truce::params::SmoothingStyle::None }
}

/// Generate a constructor call for a field with `#[param(...)]` attributes.
fn gen_field_constructor(f: &ParamField) -> proc_macro2::TokenStream {
    let a = &f.attrs;

    let id = a.id.unwrap_or(0);
    let name = a.name.as_deref().unwrap_or("Unnamed");
    let short_name = a.short_name.as_deref().unwrap_or(name);
    let group = a.group.as_deref().unwrap_or("");
    let default_plain = a.default.unwrap_or(0.0);

    let range = match &a.range {
        Some(r) => parse_range_tokens(r),
        None => match f.kind {
            ParamKind::Bool => quote! { ::truce::params::ParamRange::Discrete { min: 0, max: 1 } },
            ParamKind::Enum => {
                // Auto-infer variant count from the enum type's ParamEnum impl
                if let Some(ref enum_ty) = f.enum_type {
                    quote! { ::truce::params::ParamRange::Enum { count: <#enum_ty as ::truce::params::ParamEnum>::variant_count() } }
                } else {
                    quote! { ::truce::params::ParamRange::Enum { count: 2 } }
                }
            }
            _ => quote! { ::truce::params::ParamRange::Linear { min: 0.0, max: 1.0 } },
        },
    };

    let unit = match &a.unit {
        Some(u) => parse_unit_tokens(u),
        None => quote! { ::truce::params::ParamUnit::None },
    };

    let flags = match &a.flags {
        Some(fl) => parse_flags_tokens(fl),
        None => quote! { ::truce::params::ParamFlags::AUTOMATABLE },
    };

    let info = quote! {
        ::truce::params::ParamInfo {
            id: #id,
            name: #name,
            short_name: #short_name,
            group: #group,
            range: #range,
            default_plain: #default_plain,
            flags: #flags,
            unit: #unit,
        }
    };

    match f.kind {
        ParamKind::Float => {
            let smooth = match &a.smooth {
                Some(s) => parse_smooth_tokens(s),
                None => quote! { ::truce::params::SmoothingStyle::None },
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

#[proc_macro_derive(Params, attributes(param, nested, meter))]
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
    //
    // Keep this in sync with `truce_params::METER_ID_BASE`. The
    // proc-macro can't read the constant from truce-params at
    // expansion time, so the literal is duplicated here.
    const METER_ID_BASE: u32 = 1 << 24;
    {
        let mut next_meter = METER_ID_BASE;
        for m in &mut meter_fields {
            m.id = Some(next_meter);
            next_meter += 1;
        }
    }

    // --- Compile-time validation: duplicate IDs + range overlap ---
    //
    // Checks:
    //  1. No two params share an ID.
    //  2. No explicit param ID lands in the meter range (≥ METER_ID_BASE).
    //     Auto-assigned params can't hit this — you'd need 16M fields.
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
            if let Some(id) = m.id {
                if !seen_ids.insert(id) {
                    let msg = format!("Meter ID {id} collides with a parameter ID.");
                    return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
                }
            }
        }
    }

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
        quote! {
            let mut infos = vec![#(#own_infos),*];
            #(infos.extend(self.#nested_idents.param_infos());)*
            infos
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
            ParamKind::Float => quote! { x if x == self.#ident.id() => Some(self.#ident.value() as f64), },
            ParamKind::Bool => quote! { x if x == self.#ident.id() => Some(if self.#ident.value() { 1.0 } else { 0.0 }), },
            ParamKind::Int => quote! { x if x == self.#ident.id() => Some(self.#ident.value() as f64), },
            ParamKind::Enum => quote! { x if x == self.#ident.id() => Some(self.#ident.index() as f64), },
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
                    ParamKind::Enum => quote! {
                        x if x == self.#ident.id() => {
                            Some(self.#ident.format_by_index(value))
                        }
                    },
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
            quote! { self.#ident.smoother.snap(self.#ident.value() as f64); }
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
                let id = m.id.unwrap();
                quote! { #ident: ::truce::params::MeterSlot::new(#id) }
            })
            .collect();

        quote! {
            impl #struct_name {
                pub fn new() -> Self {
                    Self {
                        #(#param_inits,)*
                        #(#meter_inits,)*
                    }
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
        let enum_name = syn::Ident::new(&format!("{}ParamId", struct_name), struct_name.span());

        let param_variants: Vec<_> = param_fields
            .iter()
            .map(|f| {
                let variant = snake_to_pascal(&f.ident);
                let id = f.attrs.id.unwrap();
                let id_lit = proc_macro2::Literal::u32_unsuffixed(id);
                quote! { #variant = #id_lit }
            })
            .collect();

        let meter_variants: Vec<_> = meter_fields
            .iter()
            .map(|m| {
                let variant = snake_to_pascal(&m.ident);
                let id = m.id.unwrap();
                let id_lit = proc_macro2::Literal::u32_unsuffixed(id);
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
                let id = f.attrs.id.unwrap();
                let id_lit = proc_macro2::Literal::u32_unsuffixed(id);
                quote! { #id_lit => Some(#enum_name::#variant) }
            })
            .chain(meter_fields.iter().map(|m| {
                let variant = snake_to_pascal(&m.ident);
                let id = m.id.unwrap();
                let id_lit = proc_macro2::Literal::u32_unsuffixed(id);
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

    let expanded = quote! {
        #new_impl

        #param_id_enum

        impl ::truce::params::Params for #struct_name {
            fn param_infos(&self) -> Vec<::truce::params::ParamInfo> {
                #infos_expr
            }

            fn count(&self) -> usize {
                #count_expr
            }

            fn meter_ids(&self) -> Vec<u32> {
                #meter_ids_expr
            }

            fn get_normalized(&self, id: u32) -> Option<f64> {
                let info = self.param_infos().into_iter().find(|i| i.id == id)?;
                Some(info.range.normalize(self.get_plain(id)?))
            }

            fn set_normalized(&self, id: u32, value: f64) {
                if let Some(info) = self.param_infos().into_iter().find(|i| i.id == id) {
                    self.set_plain(id, info.range.denormalize(value));
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
                let mut values: Vec<f64> = ids.iter().map(|id| self.get_plain(*id).unwrap()).collect();
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

            fn default_for_gui() -> Self
            where
                Self: Sized,
            {
                Self::new()
            }
        }
    };

    expanded.into()
}

/// Convert a snake_case field name to a PascalCase enum variant ident.
fn snake_to_pascal(ident: &syn::Ident) -> syn::Ident {
    let s = ident.to_string();
    let pascal: String = s
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
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
#[proc_macro_derive(ParamEnum, attributes(name))]
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
                if attr.path().is_ident("name") {
                    if let Ok(syn::MetaNameValue {
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

    let name_strs: Vec<_> = variant_names.iter().map(|n| n.as_str()).collect();

    let expanded = quote! {
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
// #[derive(State)] — binary serialization for custom plugin state
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

    let field_count = fields.len() as u32;
    let field_idents: Vec<_> = fields.iter().map(|f| f.ident.as_ref().unwrap()).collect();

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
                if let Some(len_bytes) = cursor.read_bytes(4) {
                    let field_len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
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
                let stored_count = u32::from_le_bytes(count_bytes.try_into().ok()?) as usize;
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
