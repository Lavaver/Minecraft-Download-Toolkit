# MDT — Minecraft Download Toolkit

Download and install Minecraft versions and mod loaders from Rust.

[![Crates.io](https://img.shields.io/crates/v/mdt-minecraft.svg?style=flat-square)](https://crates.io/crates/mdt-minecraft)
[![docs.rs](https://docs.rs/mdt-minecraft/badge.svg)](https://docs.rs/mdt-minecraft)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)

**语言 / Language:** [English](README.md) · [简体中文](README.zh_CN.md)

MDT is a small library that fills a `.minecraft` folder for you — vanilla versions, mod
loaders, libraries, assets. It's the plumbing a launcher sits on top of: no UI, no progress
bars, no prompts. You call a function, you get a `Result`.

We've handled a lot of the behind-the-scenes work for you, hiding many of the implementation details that you would otherwise have to painstakingly parse from the Minecraft Wiki. All you need to do is call the default API to start playing, or customize your own implementation to tailor your experience before the game begins.

## Features

- Vanilla Minecraft, plus Forge, Fabric, Quilt, NeoForge, and LiteLoader.
- Isolated installs — each loader lands in its own `versions/<mc>+<Loader><ver>` folder, so a
  broken loader never touches your vanilla game.
- Uninstall — revert a loader back to pure vanilla with a SHA-1 manifest comparison.
- SHA-1 verified downloads with automatic mirror fallback when a source is slow or blocked.
- Custom mirrors via a `providers.toml` you can edit (compiled in at build time).
- No terminal dependencies in the core; `cli` and `tui` are opt-in features.

## Install

```toml
[dependencies]
mdt-minecraft = "0.6.6"
```

## Quick start

```rust
use mdt::{MdtClient, Mirror, ModLoader, full_install};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MdtClient::builder()
        .mirror(Mirror::BMCLAPI)
        .root_dir("./.minecraft")
        .build()?;

    // Installs vanilla 1.20.1 (if it isn't there yet), then Fabric — all in one line.
    full_install!(client, "1.20.1", ModLoader::Fabric, "0.15.11").await?;

    Ok(())
}
```

`install_modloader` takes a vanilla version directory (not a version string) and a copy-on-write
flag, so you can pick between an isolated copy and an in-place install:

```rust
let vanilla = client.root_dir().join("versions").join("1.20.1");

// Copy the vanilla dir into versions/1.20.1+Fabric0.15.11 and install there.
client.install_modloader(&vanilla, ModLoader::Fabric, "0.15.11", true).await?;

// Install directly inside versions/1.20.1 (mutates it).
client.install_modloader(&vanilla, ModLoader::Quilt, "0.24.0", false).await?;

// Revert a loader back to vanilla.
client.uninstall_modloader(&vanilla, ModLoader::Quilt).await?;
```

## Isolation

Each loader install goes into its own directory, separate from the vanilla version:

```
.minecraft/
└── versions/
    ├── 1.20.1/                  # vanilla — never touched by a loader install
    └── 1.20.1+Fabric0.15.11/    # isolated Fabric install
```


You don't have to enforce this yourself — the library does it.

## Mirrors

| Variant                  | What it is                                                                       |
|--------------------------|----------------------------------------------------------------------------------|
| `Mirror::BMCLAPI`        | Community mirror (default). Proxies Mojang, Fabric, Forge, NeoForge, LiteLoader. |
| `Mirror::Mojang`         | Official servers.                                                                |
| `Mirror::Custom("name")` | Your own provider, declared as `[name]` in `providers.toml`.                     |

Failed requests fall back to the opposite mirror automatically (custom mirrors fall back to
`BMCLAPI`). Endpoints and rewrite rules live in [`providers.toml`](providers.toml) — see
[docs/mirror_toml.md](docs/mirror_toml.md) and [docs/mirror_uri.md](docs/mirror_uri.md).

## Loaders

| Loader                  | Method                  | Notes                                        |
|-------------------------|-------------------------|----------------------------------------------|
| `ModLoader::Forge`      | A (1.13+) / B (1.12.2-) | Official installer + Java, or manual unpack. |
| `ModLoader::Neoforge`   | A                       | Official installer + Java.                   |
| `ModLoader::Fabric`     | B                       | No Java needed.                              |
| `ModLoader::Quilt`      | B                       | No Java needed.                              |
| `ModLoader::Liteloader` | B                       | 1.12.2 and earlier.                          |

Method A needs a local Java runtime (`JAVA_HOME` or `java` on `PATH`).

## Errors

Every fallible call returns `Result<T, MdtError>`, so you can match on exactly what went wrong:

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

## Macros

A few one-liners for common things:

```rust
use mdt::{version_dir, download_task};

let dir  = version_dir!(client, "1.16.5", Fabric, "0.20.0");  // <root>/versions/1.16.5+Fabric0.20.0
let task = download_task!("https://…/lib.jar", "libraries/lib.jar", "abc123", 42);
```

### `full_install!`

`full_install!` is the whole install in one line — it installs the vanilla version (if it isn't
already on disk), then the loader, so you don't have to call `install_vanilla` separately:

```rust
use mdt::{MdtClient, Mirror, ModLoader, full_install};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MdtClient::builder().mirror(Mirror::BMCLAPI).root_dir("./.minecraft").build()?;

    // Vanilla + loader, copy-on-write (default): vanilla stays untouched, the loader lands in
    // its own directory.
    full_install!(client, "1.20.1", ModLoader::Fabric, "0.15.11").await?;

    // Pass `false` to install the loader in place, mutating the vanilla directory.
    full_install!(client, "1.20.1", ModLoader::Quilt, "0.24.0", false).await?;

    Ok(())
}
```

Under the hood `full_install!(client, "1.20.1", loader, "ver")` expands to
`client.full_install("1.20.1", loader, "ver", true)` — which installs vanilla first, then calls
[`install_modloader`](#isolation). If you already installed vanilla, the vanilla step is skipped.

## Feature flags

The core library has no terminal dependencies by default:

```toml
mdt-minecraft = "0.6.6"                                              # library only
mdt-minecraft = { version = "0.6.6", features = ["tui"] }            # + TUI
mdt-minecraft = { version = "0.6.6", features = ["cli"] }            # + CLI and TUI
```

## Docs

- [docs.rs/mdt-minecraft](https://docs.rs/mdt-minecraft)
- [Mirror TOML format](docs/mirror_toml.md)
- [Mirror URI spec](docs/mirror_uri.md)
- [The “Game Files” section in the “Tutorial: Writing a Launcher” page on the Minecraft Wiki](https://zh.minecraft.wiki/w/Tutorial:%E7%BC%96%E5%86%99%E5%90%AF%E5%8A%A8%E5%99%A8?variant=zh-cn#%E6%B8%B8%E6%88%8F%E6%96%87%E4%BB%B6)

## Changelog

See [CHANGELOG.md](CHANGELOG.md) and the [releases](https://github.com/Lavaver/MDT/releases) page.

## License

[MIT](LICENSE)
