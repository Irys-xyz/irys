use derive_syn_parse::Parse;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use std::{env, fs};
use syn::{parse2, Error, ExprStruct, Ident, LitStr, Result, Token};

#[proc_macro]
pub fn load_toml(input: TokenStream) -> TokenStream {
    match load_toml_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[derive(Parse)]
struct LoadImplArgs {
    env_var: LitStr,
    _comma: Token![,],
    default_literal: ExprStruct,
}

fn load_toml_impl(tokens: impl Into<TokenStream2>) -> Result<TokenStream2> {
    let tokens: TokenStream2 = tokens.into();

    // Parse input arguments
    let args: LoadImplArgs = parse2(tokens)?;
    let default_literal = args.default_literal;
    let env_var = args.env_var;

    let struct_ident = default_literal.path.segments.last().unwrap().ident.clone();

    // Retrieve the environment variable
    let file_path = match env::var(&env_var.value()) {
        Ok(path) => path,
        Err(_) => {
            // Use the provided default if the environment variable is not set
            return Ok(quote! {
                #default_literal
            });
        }
    };

    // Read and parse the TOML file
    let toml_content = match fs::read_to_string(&file_path) {
        Ok(content) => content,
        Err(_) => {
            return Err(Error::new_spanned(
                env_var,
                format!("Failed to read file at '{}'.", file_path),
            ));
        }
    };

    let toml_data: toml::Value = match toml_content.parse() {
        Ok(data) => data,
        Err(_) => {
            return Err(Error::new_spanned(
                env_var,
                format!("Failed to parse TOML from file at '{}'.", file_path),
            ));
        }
    };

    // Generate struct literal from TOML
    let mut fields = vec![];
    if let toml::Value::Table(table) = toml_data {
        for (key, value) in table {
            if !key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Err(Error::new_spanned(
                    env_var,
                    format!("Invalid key '{}' in TOML. Only alphanumeric and underscore characters are allowed.", key),
                ));
            }

            let field_ident = Ident::new(&key, proc_macro2::Span::call_site());

            let field_value = match value {
                toml::Value::String(s) => quote!(#s.to_string()),
                toml::Value::Integer(i) => quote!(#i),
                toml::Value::Float(f) => {
                    let f_raw: TokenStream2 = f.to_string().parse().unwrap();
                    quote!(rust_decimal_macros::dec![#f_raw])
                }
                toml::Value::Boolean(b) => quote!(#b),
                _ => {
                    return Err(Error::new_spanned(
                        env_var,
                        format!("Unsupported value for key '{}'. Nested or complex values are not allowed.", key),
                    ));
                }
            };

            fields.push(quote!(#field_ident: #field_value));
        }
    } else {
        return Err(Error::new_spanned(
            env_var,
            "Top-level TOML structure must be a table.",
        ));
    }

    let generated = quote! {
        #struct_ident {
            #( #fields, )*
        }
    };

    Ok(generated)
}

#[cfg(test)]
use syn::parse_quote;

#[cfg(test)]
use quote::ToTokens;

#[test]
fn test_load_toml_impl_01() {
    let env_var_name = "TOML_FILE_01";
    env::set_var(env_var_name, "integration/input_01.toml");

    let input = quote! {
        "TOML_FILE_01", FooBar {
            foo: 1,
            bar: false,
            fizz: "default",
            buzz: 0.0,
        }
    };

    let expected_output: ExprStruct = parse_quote! {
        FooBar {
            foo: 7i64,
            bar: true,
            fizz: "hey".to_string(),
            buzz: rust_decimal_macros::dec![3.14],
        }
    };

    let output = load_toml_impl(input).unwrap().to_token_stream();
    let output = parse2::<ExprStruct>(output).unwrap();
    assert_eq!(
        output.path.to_token_stream().to_string(),
        expected_output.path.to_token_stream().to_string()
    );

    let mut output_fields = output
        .fields
        .into_iter()
        .map(|f| f.to_token_stream().to_string())
        .collect::<Vec<_>>();
    output_fields.sort();

    let mut expected_fields = expected_output
        .fields
        .into_iter()
        .map(|f| f.to_token_stream().to_string())
        .collect::<Vec<_>>();
    expected_fields.sort();

    assert_eq!(output_fields, expected_fields);

    // Cleanup
    env::remove_var(env_var_name);
}
