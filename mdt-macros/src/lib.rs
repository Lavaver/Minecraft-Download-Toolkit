//! Proc-macro that turns `providers.compile.toml` into static lookup tables.
//!
//! Like `rust-i18n`'s `i18n!`: [`providers!`] reads `providers.compile.toml` at compile time and expands
//! to `&'static` endpoint and rewrite tables, so nothing is parsed at runtime.

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::quote;
use std::env;
use std::path::PathBuf;

/// URL fields every provider must declare, in declaration order.
///
/// Must stay in sync with `ManifestUrls` (see `src/urls.rs`).
const FIELDS: [&str; 13] = [
    "minecraft",
    "forge",
    "fabric",
    "quilt",
    "neoforge",
    "liteloader",
    "forge_maven",
    "fabric_maven",
    "quilt_maven",
    "neoforge_maven",
    "liteloader_base",
    "fabric_meta",
    "quilt_meta",
];

/// Provider names [`Mirror::provider_name`] can return; they must exist in the TOML.
const BUILTIN_PROVIDERS: [&str; 2] = ["mojang", "bmclapi"];

#[proc_macro]
pub fn providers(_input: TokenStream) -> TokenStream {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    let toml_path = PathBuf::from(manifest_dir).join("providers.compile.toml");

    let text = std::fs::read_to_string(&toml_path)
        .unwrap_or_else(|e| panic!("mdt-macros: failed to read {}: {e}", toml_path.display()));
    let root: toml::Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("mdt-macros: failed to parse {}: {e}", toml_path.display()));
    let providers = root.as_table().unwrap_or_else(|| {
        panic!("mdt-macros: {} must be a table of `[provider_name]` sections", toml_path.display())
    });

    for name in BUILTIN_PROVIDERS {
        assert!(
            providers.contains_key(name),
            "mdt-macros: providers.compile.toml is missing the built-in `[{name}]` provider"
        );
    }

    let field_idents: Vec<Ident> = FIELDS.iter().map(|f| Ident::new(f, Span::call_site())).collect();

    let mut url_arms = Vec::new();
    let mut rewrite_arms = Vec::new();

    for (name, section) in providers {
        let name = name.as_str();
        let table = section.as_table().unwrap_or_else(|| panic!("mdt-macros: `[{name}]` must be a table"));

        let values: Vec<&str> = FIELDS
            .iter()
            .map(|f| {
                table
                    .get(*f)
                    .and_then(toml::Value::as_str)
                    .unwrap_or_else(|| panic!("mdt-macros: `[{name}]` is missing required field `{f}`"))
            })
            .collect();

        url_arms.push(quote! {
            #name => Some(&ProviderUrls { #(#field_idents: #values,)* }),
        });

        let mirror = pairs_slice(&rewrite_rules(table, "mirror"));
        let maven = pairs_slice(&rewrite_rules(table, "maven"));
        let maven_exclude = strs_slice(&string_list(table, "maven_exclude"));

        rewrite_arms.push(quote! {
            #name => Some(&ProviderRewrites {
                mirror: #mirror,
                maven: #maven,
                maven_exclude: #maven_exclude,
            }),
        });
    }

    let expanded = quote! {
        // `include_str!` registers providers.compile.toml as a build input so edits trigger a
        // recompile (the stable equivalent of the still-unstable `proc_macro::tracked_path`).
        #[allow(dead_code)]
        const _PROVIDERS_TOML_SRC: &str =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/providers.compile.toml"));

        pub(crate) struct ProviderUrls {
            #(#field_idents: &'static str,)*
        }

        pub(crate) fn provider_urls(name: &str) -> Option<&'static ProviderUrls> {
            match name {
                #(#url_arms)*
                _ => None,
            }
        }

        pub(crate) struct ProviderRewrites {
            pub mirror: &'static [(&'static str, &'static str)],
            pub maven: &'static [(&'static str, &'static str)],
            pub maven_exclude: &'static [&'static str],
        }

        pub(crate) fn provider_rewrites(name: &str) -> Option<&'static ProviderRewrites> {
            match name {
                #(#rewrite_arms)*
                _ => None,
            }
        }

        const EMPTY_PAIRS: &'static [(&'static str, &'static str)] = &[];
        const EMPTY_STRS: &'static [&'static str] = &[];
    };

    expanded.into()
}

/// Read an ordered `[[name.{key}]]` array-of-tables into `(from, to)` pairs.
fn rewrite_rules(table: &toml::Table, key: &str) -> Vec<(String, String)> {
    match table.get(key) {
        None => Vec::new(),
        Some(value) => {
            let array = value
                .as_array()
                .unwrap_or_else(|| panic!("mdt-macros: `{key}` must be an array of tables"));
            array
                .iter()
                .map(|item| {
                    let t = item
                        .as_table()
                        .unwrap_or_else(|| panic!("mdt-macros: `{key}` entries must be `{{ from, to }}` tables"));
                    let from = t
                        .get("from")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_else(|| panic!("mdt-macros: `{key}` entry is missing `from`"));
                    let to = t
                        .get("to")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_else(|| panic!("mdt-macros: `{key}` entry is missing `to`"));
                    (from.to_string(), to.to_string())
                })
                .collect()
        }
    }
}

/// Read a `name.{key} = [...]` array of strings.
fn string_list(table: &toml::Table, key: &str) -> Vec<String> {
    match table.get(key) {
        None => Vec::new(),
        Some(value) => {
            let array = value
                .as_array()
                .unwrap_or_else(|| panic!("mdt-macros: `{key}` must be an array of strings"));
            array
                .iter()
                .map(|item| {
                    item.as_str()
                        .unwrap_or_else(|| panic!("mdt-macros: `{key}` entries must be strings"))
                        .to_string()
                })
                .collect()
        }
    }
}

/// Build a `&'static [(&str, &str)]` slice literal (or `EMPTY_PAIRS` when empty).
fn pairs_slice(pairs: &[(String, String)]) -> TokenStream2 {
    if pairs.is_empty() {
        quote! { EMPTY_PAIRS }
    } else {
        let entries: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                let from = from.as_str();
                let to = to.as_str();
                quote! { (#from, #to) }
            })
            .collect();
        quote! { &[#(#entries),*] }
    }
}

/// Build a `&'static [&str]` slice literal (or `EMPTY_STRS` when empty).
fn strs_slice(strs: &[String]) -> TokenStream2 {
    if strs.is_empty() {
        quote! { EMPTY_STRS }
    } else {
        let entries: Vec<_> = strs.iter().map(|s| s.as_str()).collect();
        quote! { &[#(#entries),*] }
    }
}
