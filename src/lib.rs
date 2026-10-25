//! # MDT — Minecraft Download Toolkit
//!
//! [![Crates.io](https://img.shields.io/crates/v/mdt-minecraft.svg)](https://crates.io/crates/mdt-minecraft)
//! [![Documentation](https://docs.rs/mdt-minecraft/badge.svg)](https://docs.rs/mdt-minecraft)
//! [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
//!
//! Download and install Minecraft versions and mod loaders into a clean `.minecraft` folder.
//! It's the plumbing a launcher sits on top of — no UI, no prompts, just typed functions that
//! return `Result`. Build a launcher in three lines:
//!
//! ```no_run
//! use mdt::{MdtClient, Mirror, ModLoader};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = MdtClient::builder()
//!         .mirror(Mirror::BMCLAPI)
//!         .root_dir("./.minecraft")
//!         .build()?;
//!
//!     client.install_vanilla("1.20.1").await?;
//!
//!     let vanilla_dir = client.root_dir().join("versions").join("1.20.1");
//!     client.install_modloader(&vanilla_dir, ModLoader::Fabric, "0.15.11", true).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Isolation guarantee
//!
//! Every loader install lands in `versions/<mc_version>+<Loader><loader_version>` (for example
//! `versions/1.16.5+Fabric0.20.0`). The vanilla `versions/<mc_version>` folder is never touched,
//! so a broken loader can't wreck your vanilla game.
//!
//! # Errors
//!
//! Anything that can fail returns a [`Result<T, MdtError>`][std::result::Result], so you can
//! match on the exact problem (network, checksum, missing Java, ...) instead of a boxed blob.
//! [`MdtError`] is `#[non_exhaustive]`, so keep a wildcard `_` arm.
//!
//! # Mirrors and custom providers
//!
//! Pick a source with [`Mirror`]: [`Mirror::BMCLAPI`] (the default — a community mirror that
//! proxies Mojang, Fabric, Forge, NeoForge, and LiteLoader), [`Mirror::Mojang`] (official), or
//! [`Mirror::Custom`] (your own). Endpoints and rewrite rules live in `providers.toml` and get
//! compiled into static tables by the `mdt_macros::providers!` macro, so nothing is parsed at
//! runtime.
//!
//! To add a custom mirror, drop a `[name]` section in `providers.toml`, rebuild, and pick it at
//! runtime:
//!
//! ```no_run
//! use mdt::{MdtClient, Mirror};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = MdtClient::builder()
//!         .mirror(Mirror::Custom("my_mirror".into()))
//!         .root_dir("./.minecraft")
//!         .build()?;
//!
//!     Ok(())
//! }
//! ```
//!
//! [`Mirror::provider_name`] returns the active provider's key, and failed requests fall back
//! automatically via [`Mirror::other`] (a custom mirror falls back to `BMCLAPI`). The format is
//! spelled out in [the TOML spec](https://github.com/Lavaver/MDT/blob/main/docs/mirror_toml.md)
//! and [the endpoint URI spec](https://github.com/Lavaver/MDT/blob/main/docs/mirror_uri.md).
//!
//! # Macros
//!
//! [`version_dir!`], [`download_task!`], and [`full_install!`] are one-line shortcuts for isolated
//! paths, download tasks, and loader installs.

mod client;
mod error;
mod http;
mod macros;

pub mod asset;
pub mod downloader;
pub mod installer;
pub mod manifest;
pub mod mc_version;
pub mod mirror;
pub mod modloader;
pub mod registry;
pub mod urls;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(feature = "tui")]
pub mod tui;

pub use asset::AssetRegistry;
pub use client::{MdtClient, MdtClientBuilder};
pub use downloader::{AssetIndex, AssetObject, DownloadTask, VersionJson};
pub use error::MdtError;
pub use installer::{InstallMethod, InstallRequest, InstalledVersion};
pub use manifest::{
    FabricVersionsList, ForgeVersionsList, LatestMinecraftVersion, LiteLoaderVersionEntry,
    LoaderVersionEntry, ManifestData, MavenMetadata, MinecraftVersionsList, QuiltVersionsList,
    VersionEntry, VersionType, fetch_all_manifests, loader_versions_from_data, neoforge_mc_prefix,
};
pub use mc_version::McVersion;
pub use mirror::Mirror;
pub use modloader::ModLoader;
pub use urls::ManifestUrls;
