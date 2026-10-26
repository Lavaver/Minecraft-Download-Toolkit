# Mirror provider URI specification

**语言 / Language:** [English](mirror_uri.md) · [简体中文](mirror_uri.zh_CN.md)

Each provider supplies thirteen endpoints. Every field holds an absolute URL; MDT joins
paths onto the `*_meta` / `*_maven` / `*_base` fields at call time and never appends
paths to the manifest fields.

| Field             | Endpoint / base URI                                                     |
|-------------------|-------------------------------------------------------------------------|
| `minecraft`       | Minecraft version manifest (`version_manifest.json`).                   |
| `forge`           | Forge Maven metadata (`maven-metadata.json`).                           |
| `fabric`          | Fabric version list (`/v2/versions`).                                   |
| `quilt`           | Quilt version list (`/v3/versions`).                                    |
| `neoforge`        | NeoForge Maven metadata (`maven-metadata.xml`).                         |
| `liteloader`      | LiteLoader manifest.                                                    |
| `forge_maven`     | Forge Maven repository base; `<base>/<maven-path>` fetches an artifact. |
| `fabric_maven`    | Fabric Maven repository base.                                           |
| `quilt_maven`     | Quilt Maven repository base.                                            |
| `neoforge_maven`  | NeoForge Maven repository base.                                         |
| `liteloader_base` | LiteLoader download base; `<base>/<mc>/<jar>` fetches a build.          |
| `fabric_meta`     | Fabric meta base; profiles live at `<base>/v2/versions/loader/…`.       |
| `quilt_meta`      | Quilt meta base; profiles live at `<base>/v3/versions/loader/…`.        |

See the [TOML specification](mirror_toml.md) for how these fields are declared in
`providers.toml`.
