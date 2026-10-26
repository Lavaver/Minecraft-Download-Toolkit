//! Virtual directory mounting.
//!
//! Bridges the global content-addressed registry cache and the physical `.minecraft` layout
//! without duplicating file payloads. For every file we try the cheapest link first and fall back
//! gracefully:
//!
//! 1. **Hardlink** — instant and free, but only within a single volume.
//! 2. **Symlink** — works cross-volume on Unix; on Windows it needs Developer Mode / admin.
//! 3. **Copy** — last resort (FAT32 USB, cross-drive on Windows, missing privileges).
//!
//! The registry content is never re-hashed here: hashes were already verified at download time, so
//! mounting only sets up the link.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::stream::{self, StreamExt};

use crate::error::MdtError;

/// Default concurrency for batch mounting. Bounded so we never exhaust file descriptors.
pub const DEFAULT_MOUNT_CONCURRENCY: usize = 32;

/// How many times to retry a mount operation that keeps hitting a transient file lock
/// (Windows sharing violation, error 32).
const MAX_LOCK_RETRIES: usize = 3;

/// Delay between file-lock retries.
const RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// A single virtual mount: `source` (global registry) -> `target` (local `.minecraft`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualDir {
    source: PathBuf,
    target: PathBuf,
}

impl VirtualDir {
    /// Build a virtual mount from a registry path to a launcher path.
    #[must_use]
    pub fn new(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        VirtualDir {
            source: source.into(),
            target: target.into(),
        }
    }

    /// The global-registry source path.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// The local launcher target path.
    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Materialize `target` from `source` using hardlink → symlink → copy.
    pub async fn mount(&self) -> Result<(), MdtError> {
        mount_file(&self.source, &self.target).await
    }
}

/// Mount a batch of files concurrently (bounded by `concurrency`).
pub async fn mount_all(mounts: Vec<VirtualDir>, concurrency: usize) -> Result<(), MdtError> {
    let concurrency = concurrency.max(1);
    let results = stream::iter(mounts.into_iter().map(|mount| async move {
        mount.mount().await
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

    for result in results {
        result?;
    }
    Ok(())
}

/// Mount `source` at `target`, preferring the cheapest link strategy.
async fn mount_file(source: &Path, target: &Path) -> Result<(), MdtError> {
    // The target's parent directory must exist before we link into it.
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // A symlink already pointing at the source is done — leave it alone (idempotent re-mount).
    if let Ok(link) = tokio::fs::read_link(target).await {
        if link == source {
            return Ok(());
        }
    }

    // Replace any stale target (a physical file, a wrong symlink, or a broken symlink). Removing a
    // hardlink/symlink never touches the registry source. Retry on transient Windows file locks.
    match retry_io(|| tokio::fs::remove_file(target)).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(MdtError::VirtualDirMountFailed(format!(
                "cannot replace existing target {}: {e}",
                target.display()
            )));
        }
    }

    // 1. Hardlink — instant, zero extra space; only works within one volume.
    if retry_io(|| tokio::fs::hard_link(source, target)).await.is_ok() {
        return Ok(());
    }

    // 2. Symlink — works cross-volume on Unix, needs privileges on Windows.
    if retry_io(|| symlink_file(source, target)).await.is_ok() {
        return Ok(());
    }

    // 3. Fallback to a physical copy (cross-drive Windows, FAT32, or no symlink rights).
    eprintln!(
        "[mdt] WARN: Hardlink/Symlink failed. Falling back to physical copy for {}.",
        target.display()
    );
    retry_io(|| tokio::fs::copy(source, target)).await?;
    Ok(())
}

/// Retry a fallible I/O operation while it keeps failing with a transient file-lock error
/// (Windows sharing violation, error 32). Non-lock errors and success return immediately.
async fn retry_io<T, F, Fut>(mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    let mut attempts = 0;
    loop {
        return match op().await {
            Ok(value) => Ok(value),
            Err(e) => {
                if attempts < MAX_LOCK_RETRIES && is_file_lock_error(&e) {
                    attempts += 1;
                    tokio::time::sleep(RETRY_BACKOFF).await;
                    continue;
                }
                Err(e)
            }
        }
    }
}

/// Whether an I/O error is a transient Windows file-lock (sharing violation, raw OS error 32).
/// On non-Windows platforms this is always `false`.
fn is_file_lock_error(e: &std::io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(32)
}

/// Create a file symlink using the platform-appropriate API.
#[cfg(unix)]
async fn symlink_file(source: &Path, target: &Path) -> std::io::Result<()> {
    tokio::fs::symlink(source, target).await
}

/// Create a file symlink using the platform-appropriate API.
#[cfg(windows)]
async fn symlink_file(source: &Path, target: &Path) -> std::io::Result<()> {
    tokio::fs::symlink_file(source, target).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mdt-vdir-{}-{n}", std::process::id()))
    }

    #[tokio::test]
    async fn mount_creates_target_and_is_idempotent() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");
        std::fs::write(&src, b"hello").unwrap();

        // Mount a fresh target.
        VirtualDir::new(&src, &dst).mount().await.unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");

        // Re-mounting is safe (idempotent).
        VirtualDir::new(&src, &dst).mount().await.unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn mount_replaces_stale_target() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");
        std::fs::write(&src, b"good").unwrap();
        std::fs::write(&dst, b"stale").unwrap();

        VirtualDir::new(&src, &dst).mount().await.unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"good");
        // The registry source is untouched.
        assert_eq!(std::fs::read(&src).unwrap(), b"good");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_file_lock_error_detects_windows_sharing_violation() {
        let e = std::io::Error::from_raw_os_error(32);
        #[cfg(windows)]
        assert!(is_file_lock_error(&e));
        #[cfg(not(windows))]
        assert!(!is_file_lock_error(&e));
    }
}
