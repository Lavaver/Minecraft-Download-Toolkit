# 镜像 provider URI 规范

**语言 / Language:** [English](mirror_uri.md) · [简体中文](mirror_uri.zh_CN.md)

每个 provider 提供十三个端点。每个字段都持有绝对 URL；MDT 在调用时把路径拼接到 `*_meta` / `*_maven` / `*_base` 字段上，从不在清单字段上追加路径。

| 字段               | 端点 / 基础 URI                                                        |
|-------------------|-------------------------------------------------------------------------|
| `minecraft`       | Minecraft 版本清单（`version_manifest.json`）。                         |
| `forge`           | Forge Maven 元数据（`maven-metadata.json`）。                           |
| `fabric`          | Fabric 版本列表（`/v2/versions`）。                                     |
| `quilt`           | Quilt 版本列表（`/v3/versions`）。                                      |
| `neoforge`        | NeoForge Maven 元数据（`maven-metadata.xml`）。                         |
| `liteloader`      | LiteLoader 清单。                                                       |
| `forge_maven`     | Forge Maven 仓库基础路径；`<base>/<maven-path>` 获取一个产物。          |
| `fabric_maven`    | Fabric Maven 仓库基础路径。                                             |
| `quilt_maven`     | Quilt Maven 仓库基础路径。                                              |
| `neoforge_maven`  | NeoForge Maven 仓库基础路径。                                           |
| `liteloader_base` | LiteLoader 下载基础路径；`<base>/<mc>/<jar>` 获取一个构建。             |
| `fabric_meta`     | Fabric meta 基础路径；profile 位于 `<base>/v2/versions/loader/…`。      |
| `quilt_meta`      | Quilt meta 基础路径；profile 位于 `<base>/v3/versions/loader/…`。       |

这些字段如何在 `providers.compile.toml` 里声明，见 [TOML 规范](mirror_toml.zh_CN.md)。
