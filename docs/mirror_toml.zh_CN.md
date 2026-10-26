# 镜像 provider TOML 规范

**语言 / Language:** [English](mirror_toml.md) · [简体中文](mirror_toml.zh_CN.md)

Provider 的端点与 URL 重写规则在 [`providers.toml`](../providers.toml) 里声明，这是唯一事实来源。`mdt-macros::providers!` 过程宏在编译期解析它，并展开成 `&'static` 查找表（思路同 `rust-i18n` 的 `i18n!`），所以运行时什么都不读取、不解析。

## 文件结构

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

# 可选：`apply_maven_mirror` 必须原样保留其 URL 的子串。
maven_exclude = ["maven.quiltmc.org"]

# 可选：由 `ManifestUrls::apply_mirror` 按顺序应用的 URL 重写。
[[provider_name.mirror]]
from = "https://libraries.minecraft.net"
to   = "https://…/maven"

# 可选：由 `ManifestUrls::apply_maven_mirror` 按顺序应用的 URL 重写。
[[provider_name.maven]]
from = "https://maven.fabricmc.net"
to   = "https://…/maven"
```

## 规则

- 顶层必须是 `[provider_name]` 段构成的表；段名就是 [`Mirror::provider_name`] 返回的 provider 名。
- 每个 provider 的十三个 URL 字段全部必填。缺字段会让构建以清晰的报错失败。
- `mojang` 和 `bmclapi` 是内置 provider，必须始终存在。它们支撑着 [`Mirror`] 的两个变体。
- 要注册自定义镜像，添加一个 `[name]` 段，并在运行时用 `Mirror::Custom("name")` 选取。[`Mirror::provider_name`] 会返回该名称；未知名称会让 [`ManifestUrls::try_new`] 返回 [`MdtError::UnknownProvider`]。

## URL 重写

`[[provider_name.mirror]]` 和 `[[provider_name.maven]]` 是有序的 `{ from, to }` 键值对数组。它们分别驱动 [`ManifestUrls::apply_mirror`] 和 [`ManifestUrls::apply_maven_mirror`]；每个 `from` 子串按声明顺序替换为 `to`，所以较长前缀必须排在共享它的较短前缀之前（例如 `files.minecraftforge.net/maven` 要排在 `files.minecraftforge.net` 之前）。

`maven_exclude` 列出那些 `apply_maven_mirror` 要原样返回其 URL 的子串——例如 `maven.quiltmc.org`，BMCLAPI 并不代理它。

没有重写表的 provider（例如 `mojang`）会原样保留 URL。

## 字段含义

每个 URL 字段指向什么，见 [URI 规范](mirror_uri.zh_CN.md)。

[`Mirror`]: ../src/mirror.rs
[`Mirror::provider_name`]: ../src/mirror.rs
[`ManifestUrls::apply_mirror`]: ../src/urls.rs
[`ManifestUrls::apply_maven_mirror`]: ../src/urls.rs
