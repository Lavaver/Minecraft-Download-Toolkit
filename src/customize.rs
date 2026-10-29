//! Low-level customization traits.
//!
//! MDT is a "Swiss army knife": every stage of a download/install can be swapped for your own
//! implementation by implementing one of these traits and handing it to [`MdtClientBuilder`] (or,
//! for the lower-level pieces, passing it directly to the relevant function).

use std::future::Future;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::error::{io_error, MdtError};

// ===== progress reporting =====

/// Reports status and download progress to the user.
///
/// A [`ProgressReporter`] receives coarse-grained events; how they are rendered is entirely up to
/// the implementation (a logger, a TUI progress bar, a silent sink, ...). The default
/// [`StderrReporter`] reproduces MDT's Cargo-style stderr output, keeping the `Downloading x/y`
/// line pinned to the bottom of a terminal.
pub trait ProgressReporter: Send + Sync {
    /// A one-off status line, e.g. `Downloaded client.jar by Minecraft 1.20.1`.
    fn status(&self, line: &str);
    /// A batch download-progress update (`done` of `total` files finished).
    fn progress(&self, done: usize, total: usize);
    /// The current batch is finished; flush any pinned progress line.
    fn finish(&self);
    /// A byte-aware progress update: `done` of `total` files, with the bytes still outstanding.
    ///
    /// Has an empty default so existing implementations keep compiling unchanged.
    fn progress_bytes(&self, _done: usize, _total: usize, _remaining_bytes: u64) {}
    /// A batch summary, emitted once when the whole download finishes.
    ///
    /// Has an empty default so existing implementations keep compiling unchanged.
    fn summary(
        &self,
        _downloaded_count: usize,
        _total_bytes: u64,
        _elapsed: std::time::Duration,
        _largest: Option<(&str, u64)>,
    ) {
    }
}

/// Format a byte count as a human-readable size, e.g. `102.5 MiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit - 1])
}

/// Format a duration compactly, e.g. `1m23s` or `1h2m3s`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m{}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

/// The default [`ProgressReporter`]: Cargo-style output on stderr.
///
/// On a terminal the `Downloading x/y Component(s) ...` line is rewritten in place with `\r` and
/// `status` lines are printed above it; when stderr is redirected, every event is a plain line.
pub struct StderrReporter {
    inner: Mutex<StderrState>,
}

#[derive(Default)]
struct StderrState {
    interactive: bool,
    line: Option<String>,
}

impl StderrReporter {
    /// Create a reporter that writes to stderr.
    #[must_use]
    pub fn new() -> Self {
        StderrReporter {
            inner: Mutex::new(StderrState {
                interactive: std::io::stderr().is_terminal(),
                line: None,
            }),
        }
    }
}

impl Default for StderrReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressReporter for StderrReporter {
    fn status(&self, line: &str) {
        let state = self.inner.lock().unwrap();
        if state.interactive {
            if let Some(current) = &state.line {
                let width = current.chars().count();
                eprint!("\r{}\r", " ".repeat(width));
            }
            eprintln!("{line}");
            if let Some(current) = &state.line {
                eprint!("{current}");
            }
        } else {
            eprintln!("{line}");
        }
    }

    fn progress(&self, done: usize, total: usize) {
        let mut state = self.inner.lock().unwrap();
        let line = format!("Downloading {done}/{total} Component(s) ...");
        if state.interactive {
            eprint!("\r{line}");
        } else {
            eprintln!("{line}");
        }
        state.line = Some(line);
    }

    fn finish(&self) {
        let state = self.inner.lock().unwrap();
        if state.interactive {
            eprintln!();
        }
    }

    fn progress_bytes(&self, done: usize, total: usize, remaining_bytes: u64) {
        let mut state = self.inner.lock().unwrap();
        let line = format!(
            "Downloading {done}/{total} component(s), remaining bytes: {}",
            format_bytes(remaining_bytes)
        );
        if state.interactive {
            eprint!("\r{line}");
        } else {
            eprintln!("{line}");
        }
        state.line = Some(line);
    }

    fn summary(
        &self,
        downloaded_count: usize,
        total_bytes: u64,
        elapsed: Duration,
        largest: Option<(&str, u64)>,
    ) {
        let line = match largest {
            Some((name, size)) => format!(
                "Downloaded {downloaded_count} component(s) ({}) in {} (largest was `{name}` at {})",
                format_bytes(total_bytes),
                format_duration(elapsed),
                format_bytes(size)
            ),
            None => format!(
                "Downloaded {downloaded_count} component(s) ({}) in {}",
                format_bytes(total_bytes),
                format_duration(elapsed)
            ),
        };
        let mut state = self.inner.lock().unwrap();
        if state.interactive {
            if let Some(current) = &state.line {
                let width = current.chars().count();
                eprint!("\r{}\r", " ".repeat(width));
            }
        }
        eprintln!("{line}");
        state.line = None;
    }
}

// ===== transport =====

/// The raw transport: fetches URLs.
///
/// `download` streams a URL into a local file (the bulk path — assets, libraries, JARs), and
/// `fetch` reads a URL into a string (metadata — manifests, loader profiles, checksums). Neither
/// performs checksum verification; the caller decides when to verify. A non-2xx response must be
/// surfaced as [`MdtError::NetworkError`].
pub trait Downloader: Send + Sync {
    /// Stream `url` into `dest`, creating parent directories as needed.
    fn download<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), MdtError>> + Send + 'a>>;

    /// Read `url` into a UTF-8 string (small metadata payloads).
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, MdtError>> + Send + 'a>>;
}

/// The default [`Downloader`], backed by a [`reqwest::Client`].
pub struct HttpDownloader {
    client: reqwest::Client,
}

impl HttpDownloader {
    /// Build a downloader over an existing client.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        HttpDownloader { client }
    }
}

impl Downloader for HttpDownloader {
    fn download<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), MdtError>> + Send + 'a>> {
        let client = self.client.clone();
        let url = url.to_string();
        let dest = dest.to_path_buf();
        Box::pin(async move {
            let resp = client.get(&url).send().await?.error_for_status()?;
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let mut file = tokio::fs::File::create(&dest)
                .await
                .map_err(|e| io_error(e, "during download"))?;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                file.write_all(&chunk?)
                    .await
                    .map_err(|e| io_error(e, "during download"))?;
            }
            file.flush()
                .await
                .map_err(|e| io_error(e, "during download"))?;
            Ok(())
        })
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, MdtError>> + Send + 'a>> {
        let client = self.client.clone();
        let url = url.to_string();
        Box::pin(async move {
            let resp = client
                .get(&url)
                .timeout(Duration::from_secs(15))
                .send()
                .await?
                .error_for_status()?;
            Ok(resp.text().await?)
        })
    }
}

// ===== registry cache =====

/// A content-addressed cache backend (the global registry).
///
/// Keys are SHA-1 hex digests. The default [`FsRegistryCache`] stores each object at
/// `<root>/<hash[0..2]>/<hash>` — the standard Minecraft asset-object layout.
pub trait RegistryCache: Send + Sync {
    /// Whether `hash` is present in the cache.
    fn has(&self, hash: &str) -> bool;
    /// Read the bytes cached under `hash`, if present.
    fn get(&self, hash: &str) -> Option<Vec<u8>>;
    /// Store `data` under `hash`.
    fn put(&self, hash: &str, data: &[u8]) -> std::io::Result<()>;
}

/// The default [`RegistryCache`], backed by the filesystem.
pub struct FsRegistryCache {
    root: PathBuf,
}

impl FsRegistryCache {
    /// Build a filesystem cache rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FsRegistryCache { root: root.into() }
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[..2]).join(hash)
    }
}

impl RegistryCache for FsRegistryCache {
    fn has(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    fn get(&self, hash: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(hash)).ok()
    }

    fn put(&self, hash: &str, data: &[u8]) -> std::io::Result<()> {
        let path = self.path(hash);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_reporter_emits_without_panicking() {
        let reporter = StderrReporter::new();
        reporter.status("hello");
        reporter.progress(1, 3);
        reporter.progress(3, 3);
        reporter.progress_bytes(1, 3, 1024);
        reporter.summary(3, 4096, Duration::from_secs(83), Some(("client.jar", 2048)));
        reporter.finish();
    }

    #[test]
    fn format_bytes_formats_sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(107479040), "102.5 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn format_duration_formats_compactly() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
        assert_eq!(format_duration(Duration::from_secs(83)), "1m23s");
        assert_eq!(format_duration(Duration::from_secs(3723)), "1h2m3s");
    }

    #[test]
    fn fs_registry_cache_roundtrips() {
        let dir = std::env::temp_dir().join(format!("mdt-cache-test-{}", std::process::id()));
        let cache = FsRegistryCache::new(&dir);
        let hash = "0123456789abcdef0123456789abcdef01234567";

        assert!(!cache.has(hash));
        cache.put(hash, b"hello").unwrap();
        assert!(cache.has(hash));
        assert_eq!(cache.get(hash), Some(b"hello".to_vec()));

        std::fs::remove_dir_all(&dir).ok();
    }
}
