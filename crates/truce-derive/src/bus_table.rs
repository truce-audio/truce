//! `__truce_bus_table!`: embed a plugin's bus layouts in its binary.
//!
//! `truce::plugin!` invokes it with its own crate path and the logic type.
//! It emits one exported static, named per plugin crate
//! ([`truce_utils::bus_table::symbol_name`]), holding
//! `PluginLogic::BUS_LAYOUTS` serialized at compile time by the `const fn`
//! encoder next to `BusLayout`. A package-time tool reads it back out of the
//! built binary without running the plugin; see `truce_build::bus_table`.
//!
//! A proc macro only because the symbol name has to carry the crate's name,
//! and `macro_rules!` can't build an identifier.

use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let tokens: Vec<TokenTree> = TokenStream2::from(input).into_iter().collect();
    let Some(comma) = tokens
        .iter()
        .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ','))
    else {
        return quote! { compile_error!("__truce_bus_table!: expected `<truce path>, <logic type>`"); }
            .into();
    };
    let krate: TokenStream2 = tokens[..comma].iter().cloned().collect();
    let logic: TokenStream2 = tokens[comma + 1..].iter().cloned().collect();
    let Ok(pkg) = std::env::var("CARGO_PKG_NAME") else {
        return quote! { compile_error!("__truce_bus_table!: CARGO_PKG_NAME is unset"); }.into();
    };
    let symbol = format_ident!("{}", truce_utils::bus_table::symbol_name(&pkg));
    let layouts = quote! {
        <#logic as #krate::__reexport::truce_plugin::PluginLogicCore<Sample>>::BUS_LAYOUTS
    };
    quote! {
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        #[unsafe(no_mangle)]
        pub static #symbol: [u8; #krate::core::bus::encoded_len(#layouts)] =
            #krate::core::bus::encode(#layouts);
    }
    .into()
}
