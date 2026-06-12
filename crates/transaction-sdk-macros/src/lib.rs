use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::{ToTokens, format_ident, quote};
use std::collections::HashSet;
use syn::{
    Data, DeriveInput, Error, Fields, FnArg, ItemFn, Path, Type, WherePredicate, parse_macro_input,
    parse_quote,
};

const PERSIST_METADATA_MAGIC: &[u8; 4] = b"TPRS";
const PERSIST_METADATA_VERSION: u8 = 1;

#[proc_macro_derive(Persist)]
pub fn derive_persist(input: TokenStream) -> TokenStream {
    match derive_persist_impl(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn transaction(_args: TokenStream, input: TokenStream) -> TokenStream {
    expand_transaction_impl(parse_macro_input!(input as ItemFn)).into()
}

fn expand_transaction_impl(mut func: ItemFn) -> proc_macro2::TokenStream {
    let sdk_path = sdk_path();
    let persistent_arg_markers = func
        .sig
        .inputs
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| {
            is_mutable_persistent_arg(arg).then(|| {
                let index = index as u32;
                quote! {
                    #sdk_path::marker::mark_persistent_arg(#index);
                }
            })
        })
        .collect::<Vec<_>>();
    let persistent_bounds = persistent_bound_predicates(&func.sig.inputs, &sdk_path);
    if !persistent_bounds.is_empty() {
        func.sig
            .generics
            .make_where_clause()
            .predicates
            .extend(persistent_bounds);
    }
    let block = func.block;
    func.block = Box::new(syn::parse_quote!({
        #sdk_path::marker::mark_transaction_func();
        #(#persistent_arg_markers)*
        #block
    }));
    quote!(#func)
}

fn derive_persist_impl(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &input.generics,
            "Persist can only be derived for non-generic structs",
        ));
    }

    let data = match &input.data {
        Data::Struct(data) => data,
        _ => {
            return Err(Error::new_spanned(
                &input.ident,
                "Persist can only be derived for structs; enums and unions are unsupported",
            ));
        }
    };

    require_stable_repr(&input)?;

    let field_types = persist_field_types(&data.fields)?;

    let ident = input.ident;
    let sdk_path = sdk_path();
    let type_name = ident.to_string();
    let metadata_bytes = persist_metadata_bytes(&ident, &type_name)?;
    let metadata_len = metadata_bytes.len();
    let metadata_bytes = proc_macro2::Literal::byte_string(&metadata_bytes);
    let metadata_ident = format_ident!("__TWASM_PERSIST_METADATA_{}", ident);
    let field_assert_ident = format_ident!("__TWASM_PERSIST_FIELDS_{}", ident);
    let field_assert = if field_types.is_empty() {
        quote!()
    } else {
        quote! {
            #[allow(non_camel_case_types)]
            struct #field_assert_ident
            where
                #(#field_types: #sdk_path::Persist,)*
            ;
        }
    };

    Ok(quote! {
        #field_assert

        unsafe impl #sdk_path::Persist for #ident {
            const TYPE_NAME: &'static str = #type_name;
        }

        #[used]
        #[allow(non_upper_case_globals)]
        #[cfg_attr(target_arch = "wasm32", unsafe(link_section = "twasm.persist"))]
        static #metadata_ident: [u8; #metadata_len] = *#metadata_bytes;
    })
}

fn require_stable_repr(input: &DeriveInput) -> syn::Result<()> {
    for attr in &input.attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }

        let mut has_stable_repr = false;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("C") || meta.path.is_ident("transparent") {
                has_stable_repr = true;
            }
            Ok(())
        })?;

        if has_stable_repr {
            return Ok(());
        }
    }

    Err(Error::new_spanned(
        &input.ident,
        "Persist derive requires #[repr(C)] or #[repr(transparent)]",
    ))
}

fn persist_field_types(fields: &Fields) -> syn::Result<Vec<&Type>> {
    match fields {
        Fields::Named(fields) => Ok(fields.named.iter().map(|field| &field.ty).collect()),
        Fields::Unnamed(fields) => Ok(fields.unnamed.iter().map(|field| &field.ty).collect()),
        Fields::Unit => Ok(Vec::new()),
    }
}

fn persist_metadata_bytes(ident: &syn::Ident, type_name: &str) -> syn::Result<Vec<u8>> {
    let type_name_len = u16::try_from(type_name.len()).map_err(|_| {
        Error::new_spanned(
            ident,
            "Persist derive type names must be 65535 bytes or shorter",
        )
    })?;
    let mut metadata = Vec::with_capacity(PERSIST_METADATA_MAGIC.len() + 3 + type_name.len());
    metadata.extend_from_slice(PERSIST_METADATA_MAGIC);
    metadata.push(PERSIST_METADATA_VERSION);
    metadata.extend_from_slice(&type_name_len.to_le_bytes());
    metadata.extend_from_slice(type_name.as_bytes());
    Ok(metadata)
}

fn sdk_path() -> Path {
    match crate_name("wasmtime-transaction-sdk") {
        Ok(FoundCrate::Itself) => parse_quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = syn::Ident::new(&name, Span::call_site());
            parse_quote!(::#ident)
        }
        Err(_) => parse_quote!(::wasmtime_transaction_sdk),
    }
}

fn persistent_bound_predicates(
    inputs: &syn::punctuated::Punctuated<FnArg, syn::token::Comma>,
    sdk_path: &Path,
) -> Vec<WherePredicate> {
    let mut seen = HashSet::new();
    let mut predicates = Vec::new();

    for arg in inputs {
        let Some(predicate) = persistent_bound_predicate(arg, sdk_path) else {
            continue;
        };
        if seen.insert(predicate.to_token_stream().to_string()) {
            predicates.push(predicate);
        }
    }

    predicates
}

fn persistent_bound_predicate(arg: &FnArg, sdk_path: &Path) -> Option<WherePredicate> {
    match arg {
        FnArg::Receiver(receiver) => (receiver.reference.is_some()
            && receiver.mutability.is_some())
        .then(|| parse_quote!(Self: #sdk_path::Persist)),
        FnArg::Typed(arg) => match &*arg.ty {
            Type::Reference(reference) if reference.mutability.is_some() => {
                let elem = &reference.elem;
                Some(parse_quote!(#elem: #sdk_path::Persist))
            }
            _ => None,
        },
    }
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
    use super::{derive_persist_impl, expand_transaction_impl};
    use syn::parse_quote;

    #[test]
    fn derive_persist_preserves_metadata_symbol_spelling() {
        let mixed_case = derive_persist_impl(parse_quote! {
            #[repr(C)]
            struct Foo(u8);
        })
        .expect("derive should succeed")
        .to_string();
        let upper_case = derive_persist_impl(parse_quote! {
            #[repr(C)]
            struct FOO(u8);
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

    #[test]
    fn derive_persist_rejects_enums_with_struct_only_message() {
        let err = derive_persist_impl(parse_quote! {
            enum Choice {
                A,
                B,
            }
        })
        .expect_err("enums should be rejected");

        assert_eq!(
            err.to_string(),
            "Persist can only be derived for structs; enums and unions are unsupported"
        );
    }

    #[test]
    fn derive_persist_rejects_structs_without_stable_repr() {
        let err = derive_persist_impl(parse_quote! {
            struct Wrapper {
                field: u32,
            }
        })
        .expect_err("structs without repr(C) should be rejected");

        assert!(err.to_string().contains("Persist derive requires"));
    }

    #[test]
    fn derive_persist_accepts_stable_repr_unit_structs() {
        let tokens = derive_persist_impl(parse_quote! {
            #[repr(C)]
            struct Marker;
        })
        .expect("unit structs should be accepted")
        .to_string();

        assert!(tokens.contains("unsafe impl :: wasmtime_transaction_sdk :: Persist for Marker"));
    }

    #[test]
    fn transaction_attr_adds_persist_bounds_for_mut_refs_only() {
        let tokens = expand_transaction_impl(parse_quote! {
            fn update<T, U>(value: &mut T, readonly: &U) {}
        })
        .to_string();

        assert!(tokens.contains("where T : :: wasmtime_transaction_sdk :: Persist"));
        assert!(!tokens.contains("U : :: wasmtime_transaction_sdk :: Persist"));
    }

    #[test]
    fn transaction_attr_adds_persist_bounds_for_mut_self_methods() {
        let tokens = expand_transaction_impl(parse_quote! {
            fn update<T>(&mut self, value: &mut T) {}
        })
        .to_string();

        assert!(tokens.contains("where Self : :: wasmtime_transaction_sdk :: Persist"));
        assert!(tokens.contains("T : :: wasmtime_transaction_sdk :: Persist"));
    }
}
