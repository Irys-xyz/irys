use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn, LitInt};

/// Attribute macro that wraps an `async fn` body with periodic "still running"
/// warnings, so CI logs reveal which helper is hung when a test times out.
///
/// # Usage
/// ```ignore
/// #[diag_slow]          // warns every 5s (default)
/// #[diag_slow(3)]       // warns every 3s
/// pub async fn my_helper(&self) -> T { .. }
/// ```
///
/// Fast calls (< interval) produce zero output. Slow/hung calls emit:
/// `[DIAG_SLOW] my_helper still running after 5.001s`
#[proc_macro_attribute]
pub fn diag_slow(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);

    // Parse optional interval: #[diag_slow] or #[diag_slow(3)]
    let interval_secs: u64 = if attr.is_empty() {
        5
    } else {
        let lit = parse_macro_input!(attr as LitInt);
        lit.base10_parse::<u64>()
            .expect("diag_slow expects a u64 literal")
    };

    let fn_name = input_fn.sig.ident.to_string();
    let vis = &input_fn.vis;
    let sig = &input_fn.sig;
    let attrs = &input_fn.attrs;
    let body = &input_fn.block;

    let expanded = quote! {
        #(#attrs)*
        #vis #sig {
            let __diag_start = ::std::time::Instant::now();
            let __diag_fut = async move #body;
            ::tokio::pin!(__diag_fut);
            let mut __diag_interval = ::tokio::time::interval(
                ::std::time::Duration::from_secs(#interval_secs),
            );
            __diag_interval.tick().await; // skip first immediate tick
            loop {
                ::tokio::select! {
                    __diag_result = &mut __diag_fut => {
                        break __diag_result;
                    }
                    _ = __diag_interval.tick() => {
                        ::tracing::warn!(
                            "[DIAG_SLOW] {} still running after {:?}",
                            #fn_name,
                            __diag_start.elapsed(),
                        );
                    }
                }
            }
        }
    };

    expanded.into()
}
