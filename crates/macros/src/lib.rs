use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use std::{env, fs};
use syn::{parse2, Error, Ident, LitStr, Result, Token};

fn generate_struct_impl(tokens: impl Into<TokenStream2>) -> Result<TokenStream2> {
    let tokens = tokens.into();

    // Parse input arguments
    let env_var: LitStr = parse2(tokens.clone())?;
    parse2::<Token![,]>(tokens.clone())?;
    let struct_ident: Ident = parse2(tokens)?;

    // Retrieve the environment variable
    let file_path = match env::var(&env_var.value()) {
        Ok(path) => path,
        Err(_) => {
            return Err(Error::new_spanned(
                &env_var,
                format!("Environment variable '{}' is not set.", env_var.value()),
            ));
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
                toml::Value::Float(f) => quote!(#f),
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

#[proc_macro]
pub fn generate_struct_from_env_var(input: TokenStream) -> TokenStream {
    match generate_struct_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}
