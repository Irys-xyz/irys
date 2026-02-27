use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

/// Derive macro for integer-tagged enum serialization without using flatten.
/// This avoids SequenceMustHaveLength errors with binary formats.
///
/// For JSON: {"version": 1, "field1": "value1", "field2": "value2"}
/// For binary: Serializes as (version, inner_struct)
#[proc_macro_derive(IntegerTagged, attributes(integer_tagged))]
pub fn derive_integer_tagged(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let enum_name = &input.ident;
    let tag_name = extract_tag_name(&input.attrs).unwrap_or_else(|| "version".to_string());

    let variants = match &input.data {
        Data::Enum(data_enum) => &data_enum.variants,
        _ => panic!("IntegerTagged can only be derived for enums"),
    };

    let mut serialize_arms = Vec::new();
    let mut deserialize_json_arms = Vec::new();
    let mut deserialize_binary_arms = Vec::new();

    for variant in variants {
        let variant_name = &variant.ident;
        let version = extract_version(&variant.attrs).unwrap_or_else(|| {
            panic!(
                "Variant {} must have #[integer_tagged(version = N)]",
                variant_name
            )
        });

        let inner_type = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,
            _ => panic!("IntegerTagged only supports variants with a single unnamed field"),
        };

        // Serialization arm
        serialize_arms.push(quote! {
            #enum_name::#variant_name(inner) => {
                if serializer.is_human_readable() {
                    // JSON serialization: merge version field with inner fields
                    use ::serde::ser::SerializeMap;

                    // First convert inner to JSON to get field count and content
                    let inner_value = ::serde_json::to_value(inner)
                        .map_err(::serde::ser::Error::custom)?;

                    if let ::serde_json::Value::Object(inner_map) = inner_value {
                        let field_count = inner_map.len() + 1; // +1 for version field
                        let mut map = serializer.serialize_map(Some(field_count))?;

                        // Add version field first
                        map.serialize_entry(#tag_name, &#version)?;

                        // Add all inner fields
                        for (key, value) in inner_map {
                            map.serialize_entry(&key, &value)?;
                        }

                        map.end()
                    } else {
                        Err(::serde::ser::Error::custom("inner value must be a struct"))
                    }
                } else {
                    // Binary serialization: simple tuple (version, inner)
                    use ::serde::ser::SerializeTuple;
                    let mut tuple = serializer.serialize_tuple(2)?;
                    tuple.serialize_element(&#version)?;
                    tuple.serialize_element(inner)?;
                    tuple.end()
                }
            }
        });

        // JSON deserialization arm
        deserialize_json_arms.push(quote! {
            Some(#version) => {
                // Convert collected fields back to the inner type
                let inner_value = ::serde_json::Value::Object(__fields);
                let inner = ::serde_json::from_value(inner_value)
                    .map_err(::serde::de::Error::custom)?;
                Ok(#enum_name::#variant_name(inner))
            }
        });

        // Binary deserialization arm
        deserialize_binary_arms.push(quote! {
            #version => {
                let inner: #inner_type = seq.next_element()?
                    .ok_or_else(|| ::serde::de::Error::invalid_length(1, &self))?;
                Ok(#enum_name::#variant_name(inner))
            }
        });
    }

    let tag_name_str = tag_name;
    let expanded = quote! {
        impl ::serde::Serialize for #enum_name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                match self {
                    #(#serialize_arms)*
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for #enum_name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                use ::serde::de::{self, MapAccess, SeqAccess, Visitor};

                struct __EnumVisitor;

                impl<'de> Visitor<'de> for __EnumVisitor {
                    type Value = #enum_name;

                    fn expecting(&self, formatter: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                        formatter.write_str(concat!("an internally tagged ", stringify!(#enum_name)))
                    }

                    // JSON deserialization
                    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                    where
                        A: MapAccess<'de>,
                    {
                        let mut __version: Option<u8> = None; // version is a u8
                        let mut __fields = ::serde_json::Map::new();

                        // Collect all fields
                        while let Some(key) = map.next_key::<String>()? {
                            if key == #tag_name_str {
                                __version = Some(map.next_value()?);
                            } else {
                                let value: ::serde_json::Value = map.next_value()?;
                                __fields.insert(key, value);
                            }
                        }

                        // Match on version and deserialize
                        match __version {
                            #(#deserialize_json_arms)*
                            Some(v) => {
                                Err(::serde::de::Error::custom(
                                    format!("unknown {}: {}", #tag_name_str, v)
                                ))
                            }
                            None => {
                                Err(::serde::de::Error::missing_field(#tag_name_str))
                            }
                        }
                    }

                    // Binary deserialization
                    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                    where
                        A: SeqAccess<'de>,
                    {
                        let version: u8 = seq.next_element()?
                            .ok_or_else(|| ::serde::de::Error::invalid_length(0, &self))?;

                        match version {
                            #(#deserialize_binary_arms)*
                            v => Err(::serde::de::Error::custom(
                                format!("unknown version: {}", v)
                            ))
                        }
                    }
                }

                if deserializer.is_human_readable() {
                    deserializer.deserialize_map(__EnumVisitor)
                } else {
                    deserializer.deserialize_tuple(2, __EnumVisitor)
                }
            }
        }
    };

    TokenStream::from(expanded)
}

fn extract_tag_name(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if attr.path().is_ident("integer_tagged") {
            let result = attr.parse_args_with(|input: syn::parse::ParseStream| {
                let mut tag_name = None;
                while !input.is_empty() {
                    let key: syn::Ident = input.parse()?;
                    input.parse::<syn::Token![=]>()?;

                    if key == "tag" {
                        let value: syn::LitStr = input.parse()?;
                        tag_name = Some(value.value());
                    } else {
                        let _: syn::Lit = input.parse()?;
                    }

                    if !input.is_empty() {
                        input.parse::<syn::Token![,]>()?;
                    }
                }
                Ok(tag_name)
            });

            if let Ok(Some(tag_name)) = result {
                return Some(tag_name);
            }
        }
    }
    None
}

fn extract_version(attrs: &[syn::Attribute]) -> Option<u8> {
    for attr in attrs {
        if attr.path().is_ident("integer_tagged") {
            let result = attr.parse_args_with(|input: syn::parse::ParseStream| {
                let mut version = None;
                while !input.is_empty() {
                    let key: syn::Ident = input.parse()?;
                    input.parse::<syn::Token![=]>()?;

                    if key == "version" {
                        let value: syn::LitInt = input.parse()?;
                        version = Some(value.base10_parse::<u8>()?);
                    } else {
                        let _: syn::Lit = input.parse()?;
                    }

                    if !input.is_empty() {
                        input.parse::<syn::Token![,]>()?;
                    }
                }
                Ok(version)
            });

            if let Ok(Some(version)) = result {
                return Some(version);
            }
        }
    }
    None
}
