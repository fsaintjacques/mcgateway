//! Proc-macro crate for `mcgateway-sdk`. The only public macro is
//! [`merge_fn`], which wraps a user function and emits the C-ABI
//! exports the host expects.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, ItemFn, LitStr};

/// Turn a function `fn(entries: &[Entry<'_>]) -> MergeResult` into a
/// merge UDF by adding the host-facing C-ABI exports.
///
/// Optional attribute: `#[merge_fn(required_flags = "t")]` declares
/// meta flags the merge needs returned on reads; the host requests
/// those flags from backends before dispatching.
///
/// Only one `#[merge_fn]` per crate (enforced by symbol uniqueness of
/// the generated `mcgw_*` exports).
#[proc_macro_attribute]
pub fn merge_fn(attr: TokenStream, item: TokenStream) -> TokenStream {
    let required_flags = match parse_attrs(attr.into()) {
        Ok(v) => v,
        Err(e) => return e.into_compile_error().into(),
    };
    let user_fn: ItemFn = parse_macro_input!(item as ItemFn);
    let user_name = user_fn.sig.ident.clone();

    let flags_export = required_flags.map(|flags| {
        quote! {
            #[unsafe(no_mangle)]
            pub extern "C" fn mcgw_required_flags() -> u64 {
                const FLAGS: &str = #flags;
                ::mcgateway_sdk::__rt::pack_static_str(FLAGS)
            }
        }
    });

    let expanded: TokenStream2 = quote! {
        #user_fn

        #[unsafe(no_mangle)]
        pub extern "C" fn mcgw_abi_version() -> u32 {
            ::mcgateway_sdk::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn mcgw_alloc(size: u32, align: u32) -> u32 {
            unsafe { ::mcgateway_sdk::__rt::alloc_raw(size, align) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn mcgw_dealloc(ptr: u32, size: u32, align: u32) {
            unsafe { ::mcgateway_sdk::__rt::dealloc_raw(ptr, size, align) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn mcgw_merge(entries_ptr: u32, entries_len: u32) -> u64 {
            let entries = unsafe {
                ::mcgateway_sdk::__rt::decode_entries(entries_ptr, entries_len)
            };
            let result = #user_name(&entries);
            ::mcgateway_sdk::__rt::encode_result(result)
        }

        #flags_export
    };

    expanded.into()
}

fn parse_attrs(attr: TokenStream2) -> syn::Result<Option<String>> {
    if attr.is_empty() {
        return Ok(None);
    }
    let mut required_flags: Option<String> = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("required_flags") {
            let value: LitStr = meta.value()?.parse()?;
            required_flags = Some(value.value());
            Ok(())
        } else {
            Err(meta.error("unsupported merge_fn attribute"))
        }
    });
    syn::parse::Parser::parse2(parser, attr)?;
    Ok(required_flags)
}
