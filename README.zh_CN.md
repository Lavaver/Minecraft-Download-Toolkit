# MDT — Minecraft 下载工具箱

用 Rust 下载并安装 Minecraft 版本与模组加载器。

[![Crates.io](https://img.shields.io/crates/v/mdt-minecraft.svg?style=flat-square)](https://crates.io/crates/mdt-minecraft)
[![docs.rs](https://docs.rs/mdt-minecraft/badge.svg)](https://docs.rs/mdt-minecraft)
[![许可证: MIT](https://img.shields.io/badge/开源许可-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)

**语言 / Language:** [English](README.md) · [简体中文](README.zh_CN.md)

> [!NOTE]
> **此消息仅在简体中文版自述文件才能看到**。<br>
> 为便于位于中国大陆的爱好者在无法访问 GitHub 时能够继续访问并克隆仓库，我们已在 [AtomGit (原 GitCode)](https://gitcode.com/Lavaver/Minecraft-Download-Toolkit) 同步了该仓库的副本。<br>
> 为了避免跨平台协作问题，并提高后续更新效率，**同步仓库仅作为访问并克隆使用，不接受任何 Issues/PR。在同步仓库的项目讨论也将会被无视，因此建议不要在此讨论代码问题。**

MDT 是一个帮你填满 `.minecraft` 目录的小库——原版版本、模组加载器、依赖库、资源文件。它是启动器底下的那层「管道」：没有界面、没有进度条、没有交互提示。你调一个函数，拿到一个 `Result`。

我们帮你做了很多底层工作，隐藏了很多原本你需要在 Minecraft Wiki 当中仔细抠字眼的实现细节，你只需要调用默认实现的 API 即可开玩，或者自定义你的实现，在游戏开始前自定义你的体验

## 特性

- 原版 Minecraft，外加 Forge、Fabric、Quilt、NeoForge 和 LiteLoader。
- 隔离安装——每个加载器都装进各自的 `versions/<mc>+<Loader><ver>` 目录，坏掉的加载器绝不会碰到你的原版游戏。
- 卸载——通过 SHA-1 清单比对，把加载器还原回纯净原版。
- SHA-1 校验下载，源站慢或被墙时自动切换镜像。
- 通过可编辑的 `providers.compile.toml` 自定义镜像（编译期内置）。
- 核心库零终端依赖；`cli` 和 `tui` 是可选 feature。

## 安装

```toml
[dependencies]
mdt-minecraft = "0.6.6"
```

## 快速上手

```rust
use mdt::{MdtClient, Mirror, ModLoader, full_install};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MdtClient::builder()
        .mirror(Mirror::BMCLAPI)
        .root_dir("./.minecraft")
        .build()?;

    // 一行装完：先装原版 1.20.1（如果还没有），再装 Fabric。
    full_install!(client, "1.20.1", ModLoader::Fabric, "0.15.11").await?;

    Ok(())
}
```

`install_modloader` 接收的是一个原版版本目录（不是版本号字符串）和一个写时复制开关，你可以在隔离副本和原地安装之间二选一：

```rust
let vanilla = client.root_dir().join("versions").join("1.20.1");

// 把原版目录复制到 versions/1.20.1+Fabric0.15.11 再安装。
client.install_modloader(&vanilla, ModLoader::Fabric, "0.15.11", true).await?;

// 直接在 versions/1.20.1 内部原地安装（会改动它）。
client.install_modloader(&vanilla, ModLoader::Quilt, "0.24.0", false).await?;

// 把加载器还原回原版。
client.uninstall_modloader(&vanilla, ModLoader::Quilt).await?;
```

## 隔离

每个加载器安装都有自己的目录，与原版版本分开：

```
.minecraft/
└── versions/
    ├── 1.20.1/                  # 原版——加载器安装从不碰它
    └── 1.20.1+Fabric0.15.11/    # 隔离的 Fabric 安装
```

你不需要自己保证这一点——库已经帮你做了。

## 镜像

| 变体                     | 说明                                                                 |
|--------------------------|----------------------------------------------------------------------|
| `Mirror::BMCLAPI`        | 社区镜像（默认）。代理 Mojang、Fabric、Forge、NeoForge、LiteLoader。 |
| `Mirror::Mojang`         | 官方服务器。                                                         |
| `Mirror::Custom("name")` | 你自己的 provider，在 `providers.compile.toml` 里声明为 `[name]`。           |

请求失败会自动回退到对向镜像（自定义镜像回退到 `BMCLAPI`）。端点与重写规则写在 [`providers.compile.toml`](providers.compile.toml) 里——见 [docs/mirror_toml.md](docs/mirror_toml.md) 和 [docs/mirror_uri.md](docs/mirror_uri.md)。

## 加载器

| 加载器                  | 方式                     | 说明                            |
|-------------------------|--------------------------|---------------------------------|
| `ModLoader::Forge`      | A（1.13+）/ B（1.12.2-） | 官方安装器 + Java，或手动解包。 |
| `ModLoader::Neoforge`   | A                        | 官方安装器 + Java。             |
| `ModLoader::Fabric`     | B                        | 不需要 Java。                   |
| `ModLoader::Quilt`      | B                        | 不需要 Java。                   |
| `ModLoader::Liteloader` | B                        | 1.12.2 及更早。                 |

方式 A 需要本地 Java 运行时（`JAVA_HOME` 或 `PATH` 上的 `java`）。

## 错误

每个可能失败的调用都返回 `Result<T, MdtError>`，你可以精确匹配到底哪里出了问题：

```rust
let vanilla = client.root_dir().join("versions").join("1.20.1");
match client.install_modloader(&vanilla, ModLoader::Forge, "47.2.0", true).await {
    Ok(v)   => println!("installed {} via {}", v.id, v.method),
    Err(e)  => match e {
        mdt::MdtError::JavaNotFound          => eprintln!("install Java first"),
        mdt::MdtError::ChecksumMismatch{..}  => eprintln!("corrupt download"),
        other                                => eprintln!("{other}"),
    },
}
```

## 宏

几个常用场景的一行快捷方式：

```rust
use mdt::{version_dir, download_task};

let dir  = version_dir!(client, "1.16.5", Fabric, "0.20.0");  // <root>/versions/1.16.5+Fabric0.20.0
let task = download_task!("https://…/lib.jar", "libraries/lib.jar", "abc123", 42);
```

### `full_install!`

`full_install!` 一行装完——先装原版（如果磁盘上还没有），再装加载器，所以你不必单独调用 `install_vanilla`：

```rust
use mdt::{MdtClient, Mirror, ModLoader, full_install};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MdtClient::builder().mirror(Mirror::BMCLAPI).root_dir("./.minecraft").build()?;

    // 原版 + 加载器，写时复制（默认）：原版不动，加载器装进自己的目录。
    full_install!(client, "1.20.1", ModLoader::Fabric, "0.15.11").await?;

    // 传 `false` 则原地安装加载器，改动原版目录。
    full_install!(client, "1.20.1", ModLoader::Quilt, "0.24.0", false).await?;

    Ok(())
}
```

底层 `full_install!(client, "1.20.1", loader, "ver")` 展开为
`client.full_install("1.20.1", loader, "ver", true)`——先装原版，再调用
[`install_modloader`](#隔离)。如果你已经装过原版，原版这一步会跳过。

## Feature flags

核心库默认没有终端依赖：

```toml
mdt-minecraft = "0.6.6"                                              # 仅库
mdt-minecraft = { version = "0.6.6", features = ["tui"] }            # + TUI
mdt-minecraft = { version = "0.6.6", features = ["cli"] }            # + CLI 和 TUI
```

## 文档

- [docs.rs/mdt-minecraft](https://docs.rs/mdt-minecraft)
- [镜像 TOML 格式](docs/mirror_toml.md)
- [镜像 URI 规范](docs/mirror_uri.md)
- [Minecraft Wiki《教程:编写启动器》当中的《游戏文件》一节](https://zh.minecraft.wiki/w/Tutorial:%E7%BC%96%E5%86%99%E5%90%AF%E5%8A%A8%E5%99%A8?variant=zh-cn#%E6%B8%B8%E6%88%8F%E6%96%87%E4%BB%B6)

## 更新日志

见 [CHANGELOG.md](CHANGELOG.md) 和 [releases](https://github.com/Lavaver/MDT/releases) 页面。

## 许可证

[MIT](LICENSE)
