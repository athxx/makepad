extern crate proc_macro;
use proc_macro::TokenStream;

mod derive_bin;
use crate::derive_bin::*;

mod derive_json;
use crate::derive_json::*;

#[proc_macro_derive(EnBin)]
pub fn derive_en_bin(input: TokenStream) -> TokenStream {
    derive_en_bin_impl(input)
}

#[proc_macro_derive(DeBin)]
pub fn derive_de_bin(input: TokenStream) -> TokenStream {
    derive_de_bin_impl(input)
}

#[proc_macro_derive(EnJson, attributes(rename))]
pub fn derive_en_json(input: TokenStream) -> TokenStream {
    derive_en_json_impl(input)
}

#[proc_macro_derive(DeJson, attributes(rename))]
pub fn derive_de_json(input: TokenStream) -> TokenStream {
    derive_de_json_impl(input)
}