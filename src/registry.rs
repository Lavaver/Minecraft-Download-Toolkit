//! The global MDT registry, rooted at `$HOME/.mdt/registry` (override `MDT_HOME`).
//!
//! Cargo-style layout shared across every `.minecraft`:
//!
//! ```text
//! $HOME/.mdt/registry/
//! ├── src/<source>/<minecraftVersion>/main/   # vanilla assets (content-addressed by SHA-1)
//! ├── <Modloader>/<version>/                  # modloader assets (libraries, keyed by Maven path)
//! └── index/<source>/.cache/<hash>            # index JSON cache, keyed by SHA-1
//! ```

use std::path::PathBuf;

/// The canonical source name for vanilla Minecraft content.
pub const VANILLA_SOURCE: &str = "mojang";

/// The user's home directory, honouring `MDT_HOME` then `HOME` (Unix) / `USERPROFILE` (Windows).
#[must_use]
pub fn home_dir() -> PathBuf {
    std::env::var_os("MDT_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The global registry root: `$HOME/.mdt/registry`.
#[must_use]
pub fn registry_root() -> PathBuf {
    home_dir().join(".mdt").join("registry")
}

/// The global cache dir for vanilla assets: `src/mojang/<minecraftVersion>/main`.
#[must_use]
pub fn vanilla_asset_dir(minecraft_version: &str) -> PathBuf {
    registry_root()
        .join("src")
        .join(VANILLA_SOURCE)
        .join(minecraft_version)
        .join("main")
}

/// The global index cache dir for a source: `index/<source>/.cache`.
#[must_use]
pub fn index_cache_dir(source: &str) -> PathBuf {
    registry_root().join("index").join(source).join(".cache")
}

/// The global cache dir for a modloader + version: `<Modloader>/<version>`.
#[must_use]
pub fn modloader_dir(loader: &str, version: &str) -> PathBuf {
    registry_root().join(loader).join(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_follows_standard_structure() {
        let src = PathBuf::from("src").join("mojang").join("1.16.5").join("main");
        assert!(vanilla_asset_dir("1.16.5").ends_with(src), "{:?}", vanilla_asset_dir("1.16.5"));

        let idx = PathBuf::from("index").join("mojang").join(".cache");
        assert!(index_cache_dir("mojang").ends_with(idx), "{:?}", index_cache_dir("mojang"));

        let ml = PathBuf::from("Fabric").join("0.15.11");
        assert!(modloader_dir("Fabric", "0.15.11").ends_with(ml), "{:?}", modloader_dir("Fabric", "0.15.11"));
    }
}
