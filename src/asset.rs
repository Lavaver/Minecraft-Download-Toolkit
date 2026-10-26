//! Cargo-registry-inspired asset management.
//!
//! Mirrors Cargo's split between a *global* registry cache and a *per-project* copy. Vanilla
//! assets live under `src/mojang/<version>/main/`, and the `assetIndex` JSON itself is cached
//! under `index/mojang/.cache/<sha1>`. On install, MDT copies (never hardlinks) verified files
//! from the global registry into the launcher-facing `assets/objects/` directory.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::customize::{Downloader, ProgressReporter};
use crate::downloader::{self, AssetIndex, AssetIndexInfo, AssetObject, DownloadTask};
use crate::error::MdtError;
use crate::mirror::Mirror;
use crate::registry;
use crate::urls::ManifestUrls;

/// Default concurrency for asset downloads. Assets are many and small, so this is much higher than
/// the conservative default used for large files like the client JAR.
pub const ASSET_CONCURRENCY: usize = 16;

/// A parsed asset index plus the global registry cache and local object directory it spans.
pub struct AssetRegistry {
    /// Logical asset name -> object (hash + size), straight from the `assetIndex`.
    objects: HashMap<String, AssetObject>,
    /// `<root>/assets/objects` — the launcher-facing, per-install copy.
    objects_dir: PathBuf,
    /// `$HOME/.mdt/registry/src/mojang/<version>/main` — the global, shared, content-addressed store.
    registry_dir: PathBuf,
}

impl AssetRegistry {
    /// Fetch the `assetIndex` (via the global index cache), parse it, and sync its assets.
    ///
    /// This is the one-shot entry point: the index is loaded before any asset is downloaded, then
    /// missing assets are downloaded into the global registry and copied into `assets/objects`.
    pub async fn fetch_and_sync(
        root: &Path,
        asset_index: &AssetIndexInfo,
        minecraft_version: &str,
        downloader: &dyn Downloader,
        mirror: &Mirror,
        urls: &ManifestUrls,
        reporter: &dyn ProgressReporter,
    ) -> Result<(), MdtError> {
        let local_index = root
            .join("assets")
            .join("indexes")
            .join(format!("{}.json", asset_index.id));

        // 1. Ensure the index JSON is on disk, via the global index cache.
        let cache_dir = registry::index_cache_dir(registry::VANILLA_SOURCE);
        fetch_index_file(asset_index, &local_index, &cache_dir, downloader, mirror, urls, reporter).await?;

        // 2. Parse the index and sync its assets.
        let content = tokio::fs::read_to_string(&local_index).await?;
        let index: AssetIndex = serde_json::from_str(&content)?;
        AssetRegistry::new(index, root, minecraft_version)
            .sync(downloader, mirror, urls, reporter)
            .await
    }

    /// Load an already-downloaded asset index from `<root>/assets/indexes/<index_id>.json`.
    pub async fn load(
        root: &Path,
        index_id: &str,
        minecraft_version: &str,
    ) -> Result<Self, MdtError> {
        let path = root.join("assets").join("indexes").join(format!("{index_id}.json"));
        let content = tokio::fs::read_to_string(&path).await?;
        let index: AssetIndex = serde_json::from_str(&content)?;
        Ok(Self::new(index, root, minecraft_version))
    }

    /// Build a registry from an already-parsed [`AssetIndex`], using the default global cache.
    #[must_use]
    pub fn new(index: AssetIndex, root: &Path, minecraft_version: &str) -> Self {
        Self::with_registry_dir(index, root, registry::vanilla_asset_dir(minecraft_version))
    }

    /// Build a registry with an explicit global cache directory (for tests and custom layouts).
    #[must_use]
    pub fn with_registry_dir(
        index: AssetIndex,
        root: &Path,
        registry_dir: impl Into<PathBuf>,
    ) -> Self {
        AssetRegistry {
            objects: index.objects,
            objects_dir: root.join("assets").join("objects"),
            registry_dir: registry_dir.into(),
        }
    }

    /// The number of asset objects in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether the index has no objects.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Look up an object by its logical name (the key in the `assetIndex`), for on-demand fetching.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AssetObject> {
        self.objects.get(name)
    }

    /// The global registry cache directory.
    #[must_use]
    pub fn registry_dir(&self) -> &Path {
        &self.registry_dir
    }

    /// The global registry path for an object hash.
    fn registry_path(&self, hash: &str) -> PathBuf {
        object_path(&self.registry_dir, hash)
    }

    /// Return every object whose file is absent from the local `assets/objects` dir or whose SHA-1
    /// no longer matches. The hashing-heavy scan runs on the blocking pool.
    pub async fn missing_locally(&self) -> Result<Vec<AssetObject>, MdtError> {
        let objects = self.objects.clone();
        let objects_dir = self.objects_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<AssetObject>, MdtError> {
            let mut missing: Vec<AssetObject> = objects
                .values()
                .filter(|object| !cache_has(&object_path(&objects_dir, &object.hash), object))
                .cloned()
                .collect();
            // Deterministic order for reproducible cache locality and output.
            missing.sort_by(|a, b| a.hash.cmp(&b.hash));
            Ok(missing)
        })
        .await?
    }

    /// Make every asset available locally: download anything missing into the global registry
    /// (temp + SHA-1 verify + move), then copy each verified file into `assets/objects`.
    ///
    /// Returns [`MdtError::PartialDownloadFailed`] if any asset fails after its retries; whatever
    /// did download is still copied into place first.
    pub async fn sync(
        &self,
        downloader: &dyn Downloader,
        mirror: &Mirror,
        urls: &ManifestUrls,
        reporter: &dyn ProgressReporter,
    ) -> Result<(), MdtError> {
        let needed = self.missing_locally().await?;
        if needed.is_empty() {
            return Ok(());
        }

        // Download whatever the global registry doesn't already hold.
        let to_download: Vec<AssetObject> = needed
            .iter()
            .filter(|object| !cache_has(&self.registry_path(&object.hash), object))
            .cloned()
            .collect();

        let download_result = if to_download.is_empty() {
            Ok(())
        } else {
            let tasks = downloader::build_tasks_from_assets(&to_download, &self.registry_dir);
            downloader::execute_downloads(downloader, tasks, ASSET_CONCURRENCY, mirror, urls, reporter).await
        };

        // Copy everything now present (and verified) in the registry into the local objects dir.
        let ready: Vec<AssetObject> = needed
            .into_iter()
            .filter(|object| cache_has(&self.registry_path(&object.hash), object))
            .collect();
        self.copy_to_local(&ready).await?;

        download_result
    }

    /// Copy verified files from the global registry into the local objects dir (never hardlink).
    async fn copy_to_local(&self, objects: &[AssetObject]) -> Result<(), MdtError> {
        if objects.is_empty() {
            return Ok(());
        }
        let registry_dir = self.registry_dir.clone();
        let objects_dir = self.objects_dir.clone();
        let objects = objects.to_vec();
        tokio::task::spawn_blocking(move || -> Result<(), MdtError> {
            for object in &objects {
                let src = object_path(&registry_dir, &object.hash);
                let dst = object_path(&objects_dir, &object.hash);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&src, &dst)?;
            }
            Ok(())
        })
        .await?
    }
}

/// Fetch the `assetIndex` JSON into the global index cache (temp + SHA-1 verify + move), then copy
/// it into `local_index`.
async fn fetch_index_file(
    asset_index: &AssetIndexInfo,
    local_index: &Path,
    cache_dir: &Path,
    downloader: &dyn Downloader,
    mirror: &Mirror,
    urls: &ManifestUrls,
    reporter: &dyn ProgressReporter,
) -> Result<(), MdtError> {
    let cache_path = cache_dir.join(&asset_index.sha1);

    // Warm the local index from the global cache if it's already there (hash-verified).
    if sha1_of_file(&cache_path).is_some_and(|hash| hash == asset_index.sha1) {
        return copy_file(&cache_path, local_index).await;
    }

    // Download into the global cache (SHA-1 verified by the downloader), then copy into place.
    let task = DownloadTask {
        url: asset_index.url.clone(),
        dest_path: cache_path.clone(),
        sha1: asset_index.sha1.clone(),
        size: asset_index.size,
        extract_to: None,
        label: Some(format!("{}.json", asset_index.id)),
        by: None,
    };
    downloader::execute_downloads(downloader, vec![task], 1, mirror, urls, reporter).await?;
    copy_file(&cache_path, local_index).await
}

/// Copy `src` to `dst`, creating parent directories. Never hardlinks.
async fn copy_file(src: &Path, dst: &Path) -> Result<(), MdtError> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::copy(src, dst).await?;
    Ok(())
}

/// The content-addressed path for an object hash: `<dir>/<prefix>/<hash>`.
fn object_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(&hash[..2]).join(hash)
}

/// True if `path` exists with a size and SHA-1 matching `object` — i.e. a verified cache hit.
fn cache_has(path: &Path, object: &AssetObject) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if meta.len() != object.size {
        return false;
    }
    sha1_of_file(path).is_some_and(|hash| hash == object.hash)
}

/// Compute a file's SHA-1 synchronously, or `None` on any I/O error.
fn sha1_of_file(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::customize::{HttpDownloader, StderrReporter};
    use sha1::{Digest, Sha1};
    use std::sync::atomic::{AtomicU64, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mdt-asset-{tag}-{}-{n}", std::process::id()))
    }

    fn test_client() -> HttpDownloader {
        HttpDownloader::new(reqwest::Client::builder().no_proxy().build().unwrap())
    }

    fn object(content: &[u8]) -> AssetObject {
        AssetObject {
            hash: hex::encode(Sha1::digest(content)),
            size: content.len() as u64,
        }
    }

    fn registry(root: &Path, registry_dir: &Path, objects: Vec<(String, AssetObject)>) -> AssetRegistry {
        AssetRegistry::with_registry_dir(
            AssetIndex {
                objects: objects.into_iter().collect(),
            },
            root,
            registry_dir,
        )
    }

    #[tokio::test]
    async fn missing_locally_lists_everything_when_cache_is_cold() {
        let root = unique_dir("cold");
        let registry_dir = unique_dir("cold-registry");
        let obj = object(b"hello world");
        let reg = registry(&root, &registry_dir, vec![("a.png".to_string(), obj.clone())]);

        let missing = reg.missing_locally().await.unwrap();
        assert_eq!(missing, vec![obj]);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&registry_dir).ok();
    }

    #[tokio::test]
    async fn missing_locally_skips_warm_cache_and_flags_tampered_files() {
        let root = unique_dir("warm");
        let registry_dir = unique_dir("warm-registry");
        let good = object(b"good bytes");
        let tampered = object(b"tampered bytes");

        let reg = registry(
            &root,
            &registry_dir,
            vec![
                ("good.png".to_string(), good.clone()),
                ("bad.png".to_string(), tampered.clone()),
            ],
        );

        let objects_dir = root.join("assets").join("objects");
        let good_path = object_path(&objects_dir, &good.hash);
        std::fs::create_dir_all(good_path.parent().unwrap()).unwrap();
        std::fs::write(&good_path, b"good bytes").unwrap();

        let bad_path = object_path(&objects_dir, &tampered.hash);
        std::fs::create_dir_all(bad_path.parent().unwrap()).unwrap();
        std::fs::write(&bad_path, b"not the right bytes").unwrap();

        let missing = reg.missing_locally().await.unwrap();
        assert_eq!(missing, vec![tampered]);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&registry_dir).ok();
    }

    #[tokio::test]
    async fn sync_copies_from_global_registry_to_local_without_network() {
        let root = unique_dir("sync-local");
        let registry_dir = unique_dir("sync-registry");
        let content = b"asset bytes";
        let obj = object(content);
        let reg = registry(&root, &registry_dir, vec![("a.png".to_string(), obj.clone())]);

        // Pre-populate the GLOBAL registry cache with the correct file.
        let reg_path = object_path(&registry_dir, &obj.hash);
        std::fs::create_dir_all(reg_path.parent().unwrap()).unwrap();
        std::fs::write(&reg_path, content).unwrap();

        // `sync` should find it in the registry and copy it into the local objects dir, with no
        // network activity at all.
        let http = HttpDownloader::new(reqwest::Client::new());
        let mirror = Mirror::Mojang;
        let urls = ManifestUrls::try_new(&mirror).unwrap();
        reg.sync(&http, &mirror, &urls, &StderrReporter::default()).await.unwrap();

        let local = object_path(&root.join("assets").join("objects"), &obj.hash);
        assert_eq!(std::fs::read(&local).unwrap(), content);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&registry_dir).ok();
    }

    #[tokio::test]
    async fn fetch_index_file_warms_local_from_global_cache() {
        let root = unique_dir("index");
        let cache_dir = unique_dir("index-cache");
        let content = br#"{"objects":{}}"#.to_vec();
        let asset_index = AssetIndexInfo {
            id: "5".to_string(),
            sha1: hex::encode(Sha1::digest(&content)),
            size: content.len() as u64,
            url: "https://example.invalid/index.json".to_string(),
        };
        let local_index = root.join("assets").join("indexes").join("5.json");

        // Pre-populate the global index cache with the correct file.
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join(&asset_index.sha1), &content).unwrap();

        // Warm from cache; the unreachable URL must never be contacted.
        let http = HttpDownloader::new(reqwest::Client::new());
        let mirror = Mirror::Mojang;
        let urls = ManifestUrls::try_new(&mirror).unwrap();
        fetch_index_file(&asset_index, &local_index, &cache_dir, &http, &mirror, &urls, &StderrReporter::default())
            .await
            .unwrap();

        assert_eq!(std::fs::read(&local_index).unwrap(), content);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&cache_dir).ok();
    }

    #[tokio::test]
    async fn fetch_index_file_downloads_on_cache_miss() {
        let server = MockServer::start().await;
        let content = br#"{"objects":{}}"#.to_vec();
        let sha1 = hex::encode(Sha1::digest(&content));

        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&server)
            .await;

        let root = unique_dir("idx-miss");
        let cache_dir = unique_dir("idx-miss-cache");
        let asset_index = AssetIndexInfo {
            id: "5".to_string(),
            sha1: sha1.clone(),
            size: content.len() as u64,
            url: format!("{}/index.json", server.uri()),
        };
        let local_index = root.join("assets").join("indexes").join("5.json");

        let http = test_client();
        let mirror = Mirror::Mojang;
        let urls = ManifestUrls::try_new(&mirror).unwrap();
        fetch_index_file(&asset_index, &local_index, &cache_dir, &http, &mirror, &urls, &StderrReporter::default())
            .await
            .unwrap();

        // The index lands locally AND in the global cache (keyed by its SHA-1).
        assert_eq!(std::fs::read(&local_index).unwrap(), content);
        assert_eq!(std::fs::read(cache_dir.join(&sha1)).unwrap(), content);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&cache_dir).ok();
    }
}
