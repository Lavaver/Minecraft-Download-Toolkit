//! Game-file download and SHA-1 verification.
//!
//! Parses `version.json`, builds concurrent download tasks, verifies every file against its
//! manifest SHA-1, applies mirror rewriting, filters library rules, and extracts natives.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{self, StreamExt};
use futures::FutureExt;

use crate::customize::{Downloader, ProgressReporter};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tokio::io::AsyncReadExt;

use crate::error::MdtError;
use crate::mirror::Mirror;
use crate::urls::ManifestUrls;

// ===== 1. data structures =====

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Rule {
    pub action: String,
    pub os: Option<OsRule>,
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OsRule {
    pub name: Option<String>,
    pub arch: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct LibraryDownloads {
    pub artifact: Option<Artifact>,
    pub classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Artifact {
    pub path: Option<String>,
    pub sha1: String,
    pub size: u64,
    pub url: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: LibraryDownloads,
    pub rules: Option<Vec<Rule>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AssetIndexInfo {
    pub id: String,
    pub sha1: String,
    pub size: u64,
    pub url: String,
}

/// The parsed Minecraft `version.json` for a single version.
///
/// Returned by [`crate::MdtClient::install_vanilla`] so launchers can inspect the installed
/// version's assets, libraries, and downloads.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct VersionJson {
    pub id: String,
    #[serde(rename = "assetIndex")]
    pub asset_index: Option<AssetIndexInfo>,
    pub downloads: Option<HashMap<String, Artifact>>,
    #[serde(default)]
    pub libraries: Vec<Library>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AssetIndex {
    pub objects: HashMap<String, AssetObject>,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AssetObject {
    pub hash: String,
    pub size: u64,
}

// ===== 2. download tasks =====

/// A single file to download and verify.
#[derive(Debug, Clone)]
pub struct DownloadTask {
    pub url: String,
    pub dest_path: PathBuf,
    pub sha1: String,
    pub size: u64,
    /// If set, the downloaded archive is extracted here (used for natives).
    pub extract_to: Option<PathBuf>,
    /// Friendly file name for progress output, e.g. `client.jar`.
    pub label: Option<String>,
    /// Attribution for progress output, e.g. `Minecraft 1.16.5`.
    pub by: Option<String>,
}

// ===== 3. rule evaluation =====

/// Evaluate whether the current platform should download this library.
fn evaluate_rules(rules: &Option<Vec<Rule>>) -> bool {
    let Some(rules) = rules else {
        return true;
    };

    let mut allowed = false;
    let os_name = std::env::consts::OS;
    let os_arch = std::env::consts::ARCH;

    for rule in rules {
        let mut matches = true;

        if let Some(os) = &rule.os {
            if let Some(name) = &os.name {
                let name_match = match name.as_str() {
                    "windows" => os_name == "windows",
                    "osx" => os_name == "macos",
                    "linux" => os_name == "linux",
                    _ => false,
                };
                if !name_match {
                    matches = false;
                }
            }
            if let Some(arch) = &os.arch {
                let arch_match = match arch.as_str() {
                    "x86" => os_arch == "x86",
                    "x86_64" => os_arch == "x86_64",
                    "arm64" => os_arch == "aarch64",
                    _ => false,
                };
                if !arch_match {
                    matches = false;
                }
            }
        }

        // Features are not evaluated at download time; an unresolved feature means no match.
        if rule.features.is_some() {
            matches = false;
        }

        if matches {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

// ===== 4. download execution =====

/// Execute a batch of download tasks concurrently, verifying each against its SHA-1.
///
/// An empty task queue is treated as an error rather than a silent success. Progress is reported
/// through `reporter` (`Downloading x/y Component(s) ...` updates, throttled to ~10 per batch).
pub async fn execute_downloads(
    downloader: &dyn Downloader,
    tasks: Vec<DownloadTask>,
    concurrency: usize,
    mirror: &Mirror,
    urls: &ManifestUrls,
    reporter: &dyn ProgressReporter,
) -> Result<(), MdtError> {
    if tasks.is_empty() {
        return Err(MdtError::InvalidManifest(
            "download task queue is empty: the version manifest may be malformed".to_string(),
        ));
    }

    let concurrency = concurrency.max(1);
    let total = tasks.len();
    let report_every = (total / 10).max(1);
    reporter.progress(0, total);

    let throttle = Arc::new(Mutex::new(Throttle {
        done: 0,
        next_report: report_every,
    }));
    let failed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let results = stream::iter(tasks.into_iter().map(|task| {
        let throttle = Arc::clone(&throttle);
        let failed = Arc::clone(&failed);
        let label = task.label.clone();
        let by = task.by.clone();
        download_single_file(task, mirror, urls, downloader, failed).map(move |res| {
            let downloaded = matches!(&res, Ok(true));
            let mut t = throttle.lock().unwrap();
            t.done += 1;
            let done = t.done;
            let report = done >= t.next_report || done == total;
            if report {
                t.next_report = done + report_every;
            }
            drop(t);

            if downloaded {
                if let (Some(label), Some(by)) = (&label, &by) {
                    reporter.status(&format!("Downloaded {label} by {by}"));
                }
            }
            if report {
                reporter.progress(done, total);
            }
            res
        })
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

    reporter.finish();

    // Fatal errors (local I/O, native extraction) fail the whole batch; file-level network
    // failures were already soft-handled and recorded in `failed`.
    for res in results {
        res?;
    }

    let failed = failed.lock().unwrap();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(MdtError::PartialDownloadFailed(failed.clone()))
    }
}

/// Progress throttling state for [`execute_downloads`].
struct Throttle {
    done: usize,
    next_report: usize,
}

/// Stream a file in chunks and compute its SHA-1, avoiding loading the whole file into memory.
async fn sha1_file(path: &Path) -> Result<String, MdtError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| MdtError::IoError(e).context(format!("opening {}", path.display())))?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Download a single file and verify its SHA-1, trying a chain of URLs until one works.
///
/// Order: active mirror → opposite mirror → original upstream. One 403ing mirror won't kill the
/// download; the file is retried from the next source.
async fn download_file_stream(
    task: &DownloadTask,
    mirror: &Mirror,
    urls: &ManifestUrls,
    downloader: &dyn Downloader,
) -> Result<(), MdtError> {
    let candidates = fallback_urls(task, mirror, urls);
    download_from_candidates(task, &candidates, downloader).await
}

/// Try each candidate URL in order, verifying SHA-1, and return the first success.
async fn download_from_candidates(
    task: &DownloadTask,
    candidates: &[String],
    downloader: &dyn Downloader,
) -> Result<(), MdtError> {
    let mut last_err: Option<MdtError> = None;

    for url in candidates {
        match download_from_url(task, url, downloader).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Remove any partial file so the next candidate (or a later pre-check) doesn't
                // mistake a truncated download for a complete one.
                let _ = tokio::fs::remove_file(&task.dest_path).await;
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        MdtError::InvalidManifest(format!("no reachable source for {}", task.url))
    }))
}

/// Build the deduplicated fallback URL chain for a task.
///
/// Order: active mirror → opposite mirror → original upstream. The original URL is always the
/// last resort, so a download can still succeed if every mirror is down or misconfigured.
fn fallback_urls(task: &DownloadTask, mirror: &Mirror, urls: &ManifestUrls) -> Vec<String> {
    let other = mirror.other();
    let mut candidates = Vec::with_capacity(3);

    let primary = urls.apply_mirror(&task.url, mirror);
    if !candidates.contains(&primary) {
        candidates.push(primary);
    }

    // The opposite mirror is always built-in, but don't drop the upstream fallback if its
    // lookup fails.
    if let Ok(other_urls) = ManifestUrls::try_new(&other) {
        let secondary = other_urls.apply_mirror(&task.url, &other);
        if !candidates.contains(&secondary) {
            candidates.push(secondary);
        }
    }

    if !candidates.contains(&task.url) {
        candidates.push(task.url.clone());
    }

    candidates
}

/// Stream `url` into a temporary sibling of `task.dest_path`, verify its SHA-1, then move it into
/// place — so the final cache never holds a partial or unverified file.
async fn download_from_url(
    task: &DownloadTask,
    url: &str,
    downloader: &dyn Downloader,
) -> Result<(), MdtError> {
    let tmp = temp_path(&task.dest_path);

    let result = async {
        downloader.download(url, &tmp).await?;
        let actual = sha1_file(&tmp).await?;
        if actual != task.sha1 {
            return Err(MdtError::ChecksumMismatch {
                path: task.dest_path.display().to_string(),
                expected: task.sha1.clone(),
                actual,
            });
        }
        tokio::fs::rename(&tmp, &task.dest_path).await?;
        Ok::<_, MdtError>(())
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// The temporary sibling path used while a file is being downloaded and verified.
fn temp_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.mdt-download", dest.display()))
}

/// How many times to retry a failed download (after the initial attempt) before soft-failing it.
const MAX_DOWNLOAD_RETRIES: u32 = 3;

/// A small (0..250ms) jitter derived from the clock, used to spread out retry storms without a
/// `rand` dependency.
fn jitter_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    u64::from(nanos % 250)
}

/// Download a file with exponential backoff, deleting any partial file between attempts.
async fn download_with_retries(
    task: &DownloadTask,
    mirror: &Mirror,
    urls: &ManifestUrls,
    downloader: &dyn Downloader,
) -> Result<(), MdtError> {
    let mut last_err = None;
    for attempt in 0..=MAX_DOWNLOAD_RETRIES {
        if attempt > 0 {
            let base = Duration::from_secs(1u64 << (attempt - 1)); // 1s, 2s, 4s
            let jitter = Duration::from_millis(jitter_millis());
            tokio::time::sleep(base + jitter).await;
        }
        match download_file_stream(task, mirror, urls, downloader).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = tokio::fs::remove_file(&task.dest_path).await;
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        MdtError::InvalidManifest(format!("download failed with no error for {}", task.url))
    }))
}

/// Download a single file, skipping if it already exists with a matching SHA-1, then extract
/// natives when requested. Returns `true` when a fresh copy was actually downloaded.
///
/// File-level network failures are retried with backoff and then *soft-failed* (the URL is pushed
/// onto `failed` and `Ok(false)` is returned) so one bad asset never kills the whole batch.
async fn download_single_file(
    task: DownloadTask,
    mirror: &Mirror,
    urls: &ManifestUrls,
    downloader: &dyn Downloader,
    failed: Arc<Mutex<Vec<String>>>,
) -> Result<bool, MdtError> {
    // 1. Pre-check: skip if the file already exists and its SHA-1 matches.
    let mut need_download = true;
    if task.dest_path.exists() {
        let meta = tokio::fs::metadata(&task.dest_path).await?;
        if meta.len() == task.size {
            let hash = sha1_file(&task.dest_path).await?;
            if hash == task.sha1 {
                need_download = false;
            }
        }
        if need_download {
            tokio::fs::remove_file(&task.dest_path).await?;
        }
    }

    // 2. Download if necessary, retrying transient failures with exponential backoff.
    if need_download {
        if let Some(parent) = task.dest_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Err(e) = download_with_retries(&task, mirror, urls, downloader).await {
            eprintln!(
                "[mdt] WARN: Failed to download {} after {MAX_DOWNLOAD_RETRIES} retries ({e}). Skipping...",
                task.url
            );
            failed.lock().unwrap().push(task.url.clone());
            return Ok(false);
        }
    }

    // 3. Extract natives if requested.
    if let Some(extract_dir) = task.extract_to {
        let dest_path = task.dest_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), MdtError> {
            let file = std::fs::File::open(&dest_path)
                .map_err(|e| MdtError::IoError(e).context(format!("opening natives jar {}", dest_path.display())))?;
            let mut archive = zip::ZipArchive::new(file)?;

            for i in 0..archive.len() {
                let mut entry = archive.by_index(i)?;
                let name = entry.name()?.into_owned();

                if entry.is_dir() || name.starts_with("META-INF/") {
                    continue;
                }

                let out_path = extract_dir.join(&name);
                if let Some(p) = out_path.parent() {
                    std::fs::create_dir_all(p)?;
                }

                let mut out_file = std::fs::File::create(&out_path)?;
                std::io::copy(&mut entry, &mut out_file)?;
            }
            Ok(())
        })
        .await??;
    }

    Ok(need_download)
}

// ===== 5. task generators =====

/// The last path component of a Maven-relative path (e.g. `a/b/lib.jar` → `lib.jar`).
fn maven_file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build download tasks from a parsed version JSON (client JAR, libraries, asset index).
pub fn build_tasks_from_version_json(version_json: &VersionJson, root_dir: &Path) -> Vec<DownloadTask> {
    let mut tasks = Vec::new();
    let by = format!("Minecraft {}", version_json.id);

    // 1. client.jar
    if let Some(downloads) = &version_json.downloads {
        if let Some(client) = downloads.get("client") {
            tasks.push(DownloadTask {
                url: client.url.clone(),
                dest_path: root_dir
                    .join("versions")
                    .join(&version_json.id)
                    .join(format!("{}.jar", version_json.id)),
                sha1: client.sha1.clone(),
                size: client.size,
                extract_to: None,
                label: Some("client.jar".to_string()),
                by: Some(by.clone()),
            });
        }
    }

    // 2. libraries
    let natives_dir = root_dir
        .join("versions")
        .join(&version_json.id)
        .join(format!("{}-natives", version_json.id));
    let os_name = std::env::consts::OS;

    for lib in &version_json.libraries {
        if !evaluate_rules(&lib.rules) {
            continue;
        }

        if let Some(artifact) = &lib.downloads.artifact {
            if let Some(path) = &artifact.path {
                tasks.push(DownloadTask {
                    url: artifact.url.clone(),
                    dest_path: root_dir.join("libraries").join(path),
                    sha1: artifact.sha1.clone(),
                    size: artifact.size,
                    extract_to: None,
                    label: Some(maven_file_name(path).to_string()),
                    by: Some(by.clone()),
                });
            }
        }

        if let Some(classifiers) = &lib.downloads.classifiers {
            let native_key = match os_name {
                "windows" => "natives-windows",
                "macos" => "natives-macos",
                "linux" => "natives-linux",
                _ => "",
            };

            let native_artifact = classifiers
                .iter()
                .find(|(key, _)| key.starts_with(native_key))
                .map(|(_, art)| art);

            if let Some(art) = native_artifact {
                if let Some(path) = &art.path {
                    tasks.push(DownloadTask {
                        url: art.url.clone(),
                        dest_path: root_dir.join("libraries").join(path),
                        sha1: art.sha1.clone(),
                        size: art.size,
                        extract_to: Some(natives_dir.clone()),
                        label: Some(maven_file_name(path).to_string()),
                        by: Some(by.clone()),
                    });
                }
            }
        }
    }

    // The asset index is NOT downloaded here: `AssetRegistry::fetch_and_sync` fetches it through
    // the global `index/` cache (so it is cached and verified separately from the file downloads).

    tasks
}

/// Build download tasks for a set of asset objects into `objects_dir`.
pub fn build_tasks_from_assets(objects: &[AssetObject], objects_dir: &Path) -> Vec<DownloadTask> {
    objects
        .iter()
        .map(|object| {
            let prefix = &object.hash[..2];
            DownloadTask {
                url: format!("https://resources.download.minecraft.net/{prefix}/{}", object.hash),
                dest_path: objects_dir.join(prefix).join(&object.hash),
                sha1: object.hash.clone(),
                size: object.size,
                extract_to: None,
                // Assets are numerous; count them in the progress bar but skip per-file lines.
                label: None,
                by: None,
            }
        })
        .collect()
}

/// Build download tasks for every asset object in an asset index.
pub fn build_tasks_from_asset_index(asset_index: &AssetIndex, root_dir: &Path) -> Vec<DownloadTask> {
    let objects: Vec<AssetObject> = asset_index.objects.values().cloned().collect();
    build_tasks_from_assets(&objects, &root_dir.join("assets").join("objects"))
}

// ===== tests =====

#[cfg(test)]
mod tests {
    use super::{
        download_from_candidates, download_from_url, evaluate_rules, execute_downloads,
        fallback_urls, DownloadTask, OsRule, Rule,
    };
    use crate::customize::{HttpDownloader, ProgressReporter, StderrReporter};
    use crate::error::MdtError;
    use crate::mirror::Mirror;
    use crate::urls::ManifestUrls;
    use sha1::{Digest, Sha1};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_path(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mdt-{tag}-{}-{n}", std::process::id()))
    }

    fn sha1_hex(content: &[u8]) -> String {
        hex::encode(Sha1::digest(content))
    }

    fn test_task(url: &str, dest: &Path, content: &[u8]) -> DownloadTask {
        DownloadTask {
            url: url.to_string(),
            dest_path: dest.to_path_buf(),
            sha1: sha1_hex(content),
            size: content.len() as u64,
            extract_to: None,
            label: None,
            by: None,
        }
    }

    /// A downloader over an HTTP client that ignores any system proxy, so tests always reach the
    /// local mock server (reqwest 0.13 otherwise routes through the OS proxy, which 404s `127.0.0.1`).
    fn test_client() -> HttpDownloader {
        HttpDownloader::new(reqwest::Client::builder().no_proxy().build().unwrap())
    }

    // ===== fallback_urls =====

    #[test]
    fn fallback_urls_bmclapi_prefers_mirror_then_upstream() {
        let upstream = "https://libraries.minecraft.net/com/example/lib/1.0/lib-1.0.jar";
        let task = test_task(upstream, &unique_path("fb-bmc"), b"x");
        let urls = ManifestUrls::try_new(&Mirror::BMCLAPI).unwrap();
        let chain = fallback_urls(&task, &Mirror::BMCLAPI, &urls);

        assert_eq!(chain.len(), 2, "mirror + upstream, deduped: {chain:?}");
        assert!(
            chain[0].starts_with("https://bmclapi2.bangbang93.com/maven/"),
            "first candidate should be the mirror: {}",
            chain[0]
        );
        assert_eq!(chain[1], upstream);
    }

    #[test]
    fn fallback_urls_mojang_prefers_upstream_then_mirror() {
        let upstream = "https://libraries.minecraft.net/com/example/lib/1.0/lib-1.0.jar";
        let task = test_task(upstream, &unique_path("fb-mojang"), b"x");
        let urls = ManifestUrls::try_new(&Mirror::Mojang).unwrap();
        let chain = fallback_urls(&task, &Mirror::Mojang, &urls);

        assert_eq!(chain.len(), 2, "{chain:?}");
        assert_eq!(chain[0], upstream);
        assert!(chain[1].starts_with("https://bmclapi2.bangbang93.com/maven/"));
    }

    #[test]
    fn fallback_urls_custom_mirror_falls_back_to_bmclapi_then_upstream() {
        let upstream = "https://libraries.minecraft.net/com/example/lib/1.0/lib-1.0.jar";
        let task = test_task(upstream, &unique_path("fb-custom"), b"x");
        // The URL table itself is irrelevant to `apply_mirror` (it keys off the mirror name);
        // any valid table works here.
        let urls = ManifestUrls::try_new(&Mirror::BMCLAPI).unwrap();
        let chain = fallback_urls(&task, &Mirror::Custom("undeclared".into()), &urls);

        // An undeclared custom mirror has no rewrite rules, so its "primary" is the original
        // URL, followed by BMCLAPI (`other()`), with the upstream deduped away.
        assert_eq!(chain.len(), 2, "{chain:?}");
        assert_eq!(chain[0], upstream);
        assert!(chain[1].starts_with("https://bmclapi2.bangbang93.com/maven/"));
    }

    #[test]
    fn fallback_urls_never_contains_duplicates() {
        // Under Mojang, `apply_mirror` is the identity, so the "mirror" candidate equals the
        // original upstream; the chain must collapse them into a single entry.
        let url = "https://resources.download.minecraft.net/ab/abcdef0123456789";
        let task = test_task(url, &unique_path("fb-dedup"), b"x");
        let urls = ManifestUrls::try_new(&Mirror::Mojang).unwrap();
        let chain = fallback_urls(&task, &Mirror::Mojang, &urls);

        let mut seen = HashMap::new();
        for u in &chain {
            assert!(!seen.contains_key(u), "duplicate candidate URL: {u}");
            seen.insert(u.clone(), ());
        }
    }

    // ===== download_from_url =====

    #[tokio::test]
    async fn download_from_url_writes_and_verifies() {
        let server = MockServer::start().await;
        let content = b"hello mdt, this is a jar".to_vec();
        Mock::given(method("GET"))
            .and(path("/ok.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&server)
            .await;

        let dest = unique_path("dl-ok");
        let task = test_task(&format!("{}/ok.jar", server.uri()), &dest, &content);
        let http = test_client();

        download_from_url(&task, &task.url, &http).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), content);
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn download_from_url_403_is_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deny.jar"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let dest = unique_path("dl-403");
        let content = b"expected".to_vec();
        let task = test_task(&format!("{}/deny.jar", server.uri()), &dest, &content);
        let http = test_client();

        let err = download_from_url(&task, &task.url, &http).await.unwrap_err();
        assert!(matches!(err, MdtError::NetworkError(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn download_from_url_checksum_mismatch_removes_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tampered.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered-bytes".to_vec()))
            .mount(&server)
            .await;

        let dest = unique_path("dl-tampered");
        let content = b"expected-bytes".to_vec();
        let task = test_task(&format!("{}/tampered.jar", server.uri()), &dest, &content);
        let http = test_client();

        let err = download_from_url(&task, &task.url, &http).await.unwrap_err();
        assert!(matches!(err, MdtError::ChecksumMismatch { .. }), "got {err:?}");
        assert!(!dest.exists(), "corrupt file should be removed");
    }

    // ===== download_from_candidates (fallback loop) =====

    #[tokio::test]
    async fn download_candidates_falls_back_after_403() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/primary.jar"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let content = b"served by the fallback".to_vec();
        Mock::given(method("GET"))
            .and(path("/fallback.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&server)
            .await;

        let dest = unique_path("cands-403");
        let task = test_task(&format!("{}/primary.jar", server.uri()), &dest, &content);
        let candidates = vec![
            format!("{}/primary.jar", server.uri()),
            format!("{}/fallback.jar", server.uri()),
        ];
        let http = test_client();

        download_from_candidates(&task, &candidates, &http).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), content);
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn download_candidates_falls_back_after_checksum_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bad.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
            .mount(&server)
            .await;
        let content = b"correct-bytes".to_vec();
        Mock::given(method("GET"))
            .and(path("/good.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&server)
            .await;

        let dest = unique_path("cands-sha");
        let task = test_task(&format!("{}/bad.jar", server.uri()), &dest, &content);
        let candidates = vec![
            format!("{}/bad.jar", server.uri()),
            format!("{}/good.jar", server.uri()),
        ];
        let http = test_client();

        download_from_candidates(&task, &candidates, &http).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), content);
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn download_candidates_returns_last_error_when_all_fail() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a.jar"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/b.jar"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let dest = unique_path("cands-allfail");
        let content = b"unreachable".to_vec();
        let task = test_task(&format!("{}/a.jar", server.uri()), &dest, &content);
        let candidates = vec![
            format!("{}/a.jar", server.uri()),
            format!("{}/b.jar", server.uri()),
        ];
        let http = test_client();

        let err = download_from_candidates(&task, &candidates, &http).await.unwrap_err();
        assert!(matches!(err, MdtError::NetworkError(_)), "got {err:?}");
        assert!(!dest.exists(), "no partial file should remain");
    }

    #[tokio::test]
    async fn download_candidates_empty_list_is_invalid_manifest() {
        let dest = unique_path("cands-empty");
        let content = b"x".to_vec();
        let task = test_task("https://example.invalid/lib.jar", &dest, &content);
        let http = test_client();

        let err = download_from_candidates(&task, &[], &http).await.unwrap_err();
        assert!(matches!(err, MdtError::InvalidManifest(_)), "got {err:?}");
    }

    // ===== execute_downloads (concurrency path) =====

    #[tokio::test]
    async fn execute_downloads_downloads_and_verifies_concurrently() {
        let server = MockServer::start().await;
        let content_a = b"asset a".to_vec();
        let content_b = b"asset b".to_vec();
        Mock::given(method("GET"))
            .and(path("/a.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content_a.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/b.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content_b.clone()))
            .mount(&server)
            .await;

        let dest_a = unique_path("exec-a");
        let dest_b = unique_path("exec-b");
        let task_a = test_task(&format!("{}/a.bin", server.uri()), &dest_a, &content_a);
        let task_b = test_task(&format!("{}/b.bin", server.uri()), &dest_b, &content_b);

        let urls = ManifestUrls::try_new(&Mirror::Mojang).unwrap();
        let http = test_client();

        execute_downloads(&http, vec![task_a, task_b], 2, &Mirror::Mojang, &urls, &StderrReporter::default())
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest_a).unwrap(), content_a);
        assert_eq!(std::fs::read(&dest_b).unwrap(), content_b);
        let _ = std::fs::remove_file(&dest_a);
        let _ = std::fs::remove_file(&dest_b);
    }

    #[tokio::test]
    async fn execute_downloads_empty_tasks_is_error() {
        let urls = ManifestUrls::try_new(&Mirror::Mojang).unwrap();
        let http = test_client();
        let err = execute_downloads(&http, vec![], 3, &Mirror::Mojang, &urls, &StderrReporter::default())
            .await
            .unwrap_err();
        assert!(matches!(err, MdtError::InvalidManifest(_)), "got {err:?}");
    }

    #[derive(Default)]
    struct CollectingReporter {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl ProgressReporter for CollectingReporter {
        fn status(&self, line: &str) {
            self.events.lock().unwrap().push(format!("status:{line}"));
        }
        fn progress(&self, done: usize, total: usize) {
            self.events.lock().unwrap().push(format!("progress:{done}/{total}"));
        }
        fn finish(&self) {
            self.events.lock().unwrap().push("finish".to_string());
        }
    }

    #[tokio::test]
    async fn execute_downloads_reports_through_custom_reporter() {
        let server = MockServer::start().await;
        let content = b"asset".to_vec();
        Mock::given(method("GET"))
            .and(path("/a.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&server)
            .await;

        let dest = unique_path("reporter");
        let task = test_task(&format!("{}/a.bin", server.uri()), &dest, &content);
        let mut task = task;
        task.label = Some("a.bin".to_string());
        task.by = Some("Minecraft 1.20.1".to_string());

        let urls = ManifestUrls::try_new(&Mirror::Mojang).unwrap();
        let http = test_client();
        let reporter = CollectingReporter::default();

        execute_downloads(&http, vec![task], 1, &Mirror::Mojang, &urls, &reporter)
            .await
            .unwrap();

        let events = reporter.events.lock().unwrap();
        assert!(events.iter().any(|e| e == "status:Downloaded a.bin by Minecraft 1.20.1"), "{events:?}");
        assert!(events.iter().any(|e| e == "progress:1/1"), "{events:?}");
        assert!(events.iter().any(|e| e == "finish"), "{events:?}");

        let _ = std::fs::remove_file(&dest);
    }

    // ===== evaluate_rules =====

    #[test]
    fn evaluate_rules_no_rules_means_download() {
        assert!(evaluate_rules(&None));
    }

    #[test]
    fn evaluate_rules_allow_without_os_or_features() {
        let rules = Some(vec![Rule {
            action: "allow".into(),
            os: None,
            features: None,
        }]);
        assert!(evaluate_rules(&rules));
    }

    #[test]
    fn evaluate_rules_disallow_by_default() {
        let rules = Some(vec![Rule {
            action: "disallow".into(),
            os: None,
            features: None,
        }]);
        assert!(!evaluate_rules(&rules));
    }

    #[test]
    fn evaluate_rules_unknown_os_name_never_matches() {
        let rules = Some(vec![Rule {
            action: "allow".into(),
            os: Some(OsRule {
                name: Some("solaris".into()),
                arch: None,
            }),
            features: None,
        }]);
        assert!(!evaluate_rules(&rules));
    }

    #[test]
    fn evaluate_rules_unresolved_features_block_match() {
        let rules = Some(vec![Rule {
            action: "allow".into(),
            os: None,
            features: Some(HashMap::new()),
        }]);
        assert!(!evaluate_rules(&rules));
    }

    #[test]
    fn evaluate_rules_last_matching_rule_wins() {
        let allow = Rule {
            action: "allow".into(),
            os: None,
            features: None,
        };
        let disallow = Rule {
            action: "disallow".into(),
            os: None,
            features: None,
        };
        assert!(!evaluate_rules(&Some(vec![allow.clone(), disallow.clone()])));
        assert!(evaluate_rules(&Some(vec![disallow, allow])));
    }
}
