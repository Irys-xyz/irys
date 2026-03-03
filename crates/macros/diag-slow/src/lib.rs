use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Expr, Ident, ItemFn, LitInt, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

struct DiagSlowArgs {
    interval_secs: u64,
    state_expr: Option<Expr>,
}

impl Parse for DiagSlowArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut interval_secs = 5;
        let mut state_expr = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "interval" => {
                    let lit = input.parse::<LitInt>()?;
                    interval_secs = lit.base10_parse::<u64>()?;
                    if interval_secs == 0 {
                        return Err(syn::Error::new_spanned(lit, "interval must be > 0"));
                    }
                }
                "state" => {
                    state_expr = Some(input.parse::<Expr>()?);
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "diag_slow supports only `interval = <u64>` and `state = <expr>`",
                    ));
                }
            }

            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(Self {
            interval_secs,
            state_expr,
        })
    }
}

fn parse_diag_slow_attr(attr: TokenStream) -> syn::Result<(u64, Option<Expr>)> {
    if attr.is_empty() {
        return Ok((5, None));
    }
    if let Ok(lit) = syn::parse::<LitInt>(attr.clone()) {
        let secs = lit.base10_parse::<u64>()?;
        if secs == 0 {
            return Err(syn::Error::new_spanned(lit, "interval must be > 0"));
        }
        return Ok((secs, None));
    }
    let args = syn::parse::<DiagSlowArgs>(attr)?;
    Ok((args.interval_secs, args.state_expr))
}

/// Attribute macro that wraps an `async fn` body with periodic "still running"
/// warnings, so CI logs reveal which helper is hung when a test times out.
///
/// # Usage
/// ```ignore
/// #[diag_slow]          // warns every 5s (default)
/// #[diag_slow(3)]       // warns every 3s
/// #[diag_slow(interval = 2, state = self.debug_snapshot())]
/// pub async fn my_helper(&self) -> T { .. }
/// ```
///
/// Fast calls (< interval) produce zero output. Slow/hung calls emit:
/// `[DIAG_SLOW] my_helper still running after 5.001s`
#[proc_macro_attribute]
pub fn diag_slow(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);

    // Parse supported forms:
    // - #[diag_slow]
    // - #[diag_slow(3)]
    // - #[diag_slow(interval = 3, state = <expr>)]
    // - #[diag_slow(state = <expr>)]
    let (interval_secs, state_expr) = match parse_diag_slow_attr(attr) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };

    let fn_name = input_fn.sig.ident.to_string();
    let vis = &input_fn.vis;
    let sig = &input_fn.sig;
    let attrs = &input_fn.attrs;
    let body = &input_fn.block;
    let fut_decl = if state_expr.is_some() {
        quote! {
            // Keep captured variables available for optional state diagnostics.
            let __diag_fut = async #body;
        }
    } else {
        quote! {
            // `move` is safe here: ownership transfer doesn't conflict with
            // state diagnostics borrowing captured variables.
            let __diag_fut = async move #body;
        }
    };
    let diag_tick = if let Some(expr) = state_expr {
        quote! {
            let __diag_state = { #expr };
            ::tracing::warn!(
                "[DIAG_SLOW] {} still running after {:?}; state={:?}",
                #fn_name,
                __diag_start.elapsed(),
                __diag_state,
            );
        }
    } else {
        quote! {
            ::tracing::warn!(
                "[DIAG_SLOW] {} still running after {:?}",
                #fn_name,
                __diag_start.elapsed(),
            );
        }
    };

    let expanded = quote! {
        #(#attrs)*
        #vis #sig {
            let __diag_start = ::std::time::Instant::now();
            #fut_decl
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
                        #diag_tick
                    }
                }
            }
        }
    };

    expanded.into()
}
