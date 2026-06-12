use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse_macro_input};

#[proc_macro_derive(Persist)]
pub fn derive_persist(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = input.ident;
    let generics = input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    quote! {
        impl #impl_generics ::wasmtime_transaction_sdk::Persist for #ident #ty_generics #where_clause {}
    }
    .into()
}

#[proc_macro_attribute]
pub fn transaction(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}
