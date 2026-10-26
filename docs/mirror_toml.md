# Mirror provider TOML specification

**语言 / Language:** [English](mirror_toml.md) · [简体中文](mirror_toml.zh_CN.md)

Provider endpoints and URL-rewrite rules are declared in
[`providers.toml`](../providers.toml), which is the single source of truth. The
`mdt-macros::providers!` proc macro parses it at compile time and expands to `&'static`
lookup tables (the same idea as `rust-i18n`'s `i18n!`), so nothing is read or parsed at
runtime.

## File shape

```toml
[provider_name]
minecraft       = "https://…"
forge           = "https://…"
fabric          = "https://…"
quilt           = "https://…"
neoforge        = "https://…"
liteloader      = "https://…"
forge_maven     = "https://…"
fabric_maven    = "https://…"
quilt_maven     = "https://…"
neoforge_maven  = "https://…"
liteloader_base = "https://…"
fabric_meta     = "https://…"
quilt_meta      = "https://…"

# Optional: substrings whose URLs `apply_maven_mirror` must leave untouched.
maven_exclude = ["maven.quiltmc.org"]

# Optional: URL rewriting applied by `ManifestUrls::apply_mirror`, in order.
[[provider_name.mirror]]
from = "https://libraries.minecraft.net"
to   = "https://…/maven"

# Optional: URL rewriting applied by `ManifestUrls::apply_maven_mirror`, in order.
[[provider_name.maven]]
from = "https://maven.fabricmc.net"
to   = "https://…/maven"
```

## Rules

- The top level must be a table of `[provider_name]` sections; the section key is the
  provider name returned by [`Mirror::provider_name`].
- All thirteen URL fields are required for every provider. A missing field fails the
  build with a clear message.
- `mojang` and `bmclapi` are the built-in providers and must always be present. They
  back the two [`Mirror`] variants.
- To register a custom mirror, add another `[name]` section and select it at runtime with
  `Mirror::Custom("name")`. [`Mirror::provider_name`] returns that name; an unknown name
  makes [`ManifestUrls::try_new`] return [`MdtError::UnknownProvider`].

## URL rewriting

`[[provider_name.mirror]]` and `[[provider_name.maven]]` are ordered arrays of
`{ from, to }` pairs. They drive [`ManifestUrls::apply_mirror`] and
[`ManifestUrls::apply_maven_mirror`] respectively; each `from` substring is replaced with
`to`, in declaration order, so longer prefixes must come before shorter ones that share
them (e.g. `files.minecraftforge.net/maven` before `files.minecraftforge.net`).

`maven_exclude` lists substrings whose URLs are returned unchanged by
`apply_maven_mirror` — e.g. `maven.quiltmc.org`, which BMCLAPI does not proxy.

A provider with no rewrite tables (e.g. `mojang`) leaves URLs unchanged.

## Field meanings

See the [URI specification](mirror_uri.md) for what each URL field points to.

[`Mirror`]: ../src/mirror.rs
[`Mirror::provider_name`]: ../src/mirror.rs
[`ManifestUrls::apply_mirror`]: ../src/urls.rs
[`ManifestUrls::apply_maven_mirror`]: ../src/urls.rs
