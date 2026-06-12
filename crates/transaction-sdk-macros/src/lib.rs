use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Error, FnArg, ItemFn, Type, parse_macro_input};

#[proc_macro_derive(Persist)]
pub fn derive_persist(input: TokenStream) -> TokenStream {
    match derive_persist_impl(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn transaction(_args: TokenStream, input: TokenStream) -> TokenStream {
    let mut func = parse_macro_input!(input as ItemFn);
    let persistent_arg_markers = func
        .sig
        .inputs
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| {
            is_mutable_persistent_arg(arg).then(|| {
                let index = index as u32;
                quote! {
                    ::wasmtime_transaction_sdk::marker::mark_persistent_arg(#index);
                }
            })
        })
        .collect::<Vec<_>>();
    let block = func.block;
    func.block = Box::new(syn::parse_quote!({
        ::wasmtime_transaction_sdk::marker::mark_transaction_func();
        #(#persistent_arg_markers)*
        #block
    }));
    quote!(#func).into()
}

fn derive_persist_impl(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &input.generics,
            "Persist can only be derived for non-generic structs",
        ));
    }

    if !matches!(input.data, Data::Struct(_)) {
        return Err(Error::new_spanned(
            &input.ident,
            "Persist can only be derived for non-generic structs",
        ));
    }

    let ident = input.ident;
    let type_name = ident.to_string();
    let type_name_len = type_name.len();
    let type_name_bytes = proc_macro2::Literal::byte_string(type_name.as_bytes());
    let metadata_ident = format_ident!("__TWASM_PERSIST_METADATA_{}", ident);

    Ok(quote! {
        unsafe impl ::wasmtime_transaction_sdk::Persist for #ident {
            const TYPE_NAME: &'static str = #type_name;
        }

        #[used]
        #[allow(non_upper_case_globals)]
        #[cfg_attr(target_arch = "wasm32", unsafe(link_section = "twasm.persist"))]
        static #metadata_ident: [u8; #type_name_len] = *#type_name_bytes;
    })
}

fn is_mutable_persistent_arg(arg: &FnArg) -> bool {
    match arg {
        FnArg::Receiver(receiver) => receiver.reference.is_some() && receiver.mutability.is_some(),
        FnArg::Typed(arg) => matches!(
            &*arg.ty,
            Type::Reference(reference) if reference.mutability.is_some()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::derive_persist_impl;
    use syn::parse_quote;

    #[test]
    fn derive_persist_preserves_metadata_symbol_spelling() {
        let mixed_case = derive_persist_impl(parse_quote! {
            struct Foo;
        })
        .expect("derive should succeed")
        .to_string();
        let upper_case = derive_persist_impl(parse_quote! {
            struct FOO;
        })
        .expect("derive should succeed")
        .to_string();

        assert!(mixed_case.contains("__TWASM_PERSIST_METADATA_Foo"));
        assert!(upper_case.contains("__TWASM_PERSIST_METADATA_FOO"));
        assert_ne!(mixed_case, upper_case);
    }

    #[test]
    fn derive_persist_rejects_generic_structs() {
        let err = derive_persist_impl(parse_quote! {
            struct Wrapper<T>(T);
        })
        .expect_err("generic structs should be rejected");

        assert!(
            err.to_string()
                .contains("Persist can only be derived for non-generic structs")
        );
    }
}
