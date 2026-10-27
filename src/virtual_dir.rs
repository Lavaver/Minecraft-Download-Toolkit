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
//!
//! Windows can transiently hold a file open (most often Windows Defender mid-scan), surfacing as a
//! sharing violation (raw OS error 32) or access denied (error 5). [`mount_all`] treats those as a
//! "Cargo-style" blocking wait: failed mounts are deferred to a polling queue and retried every
//! [`POLL_INTERVAL`] for up to [`MAX_WAIT`]. A locked target is *never* deleted or truncated to get
//! around the lock — it is simply left alone and retried later.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::stream::{self, StreamExt};

use crate::error::MdtError;

/// Default concurrency for batch mounting. Bounded so we never exhaust file descriptors.
pub const DEFAULT_MOUNT_CONCURRENCY: usize = 32;

/// How long to wait between polls of the deferred file-lock queue.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Maximum total time to keep waiting for a file lock before giving up.
const MAX_WAIT: Duration = Duration::from_secs(30);

/// How many blocked file names to print in a "blocking wait" log line (the rest are elided).
const MAX_NAMES_SHOWN: usize = 3;

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
    ///
    /// Unlike [`mount_all`], this performs a single attempt with no deferred retry: a transient
    /// file lock surfaces as [`MdtError::VirtualDirMountFailed`] immediately.
    pub async fn mount(&self) -> Result<(), MdtError> {
        match mount_file(&self.source, &self.target).await {
            MountAttempt::Mounted => Ok(()),
            MountAttempt::Locked => Err(MdtError::VirtualDirMountFailed(format!(
                "file is locked by another process: {}",
                self.target.display()
            ))),
            MountAttempt::Failed(e) => Err(e),
        }
    }
}

/// The result of a single [`mount_file`] attempt.
enum MountAttempt {
    /// The target is now materialized.
    Mounted,
    /// A transient file lock blocked progress; retry this mount later.
    Locked,
    /// A permanent, non-lock failure; report it immediately.
    Failed(MdtError),
}

/// Mount a batch of files concurrently (bounded by `concurrency`).
///
/// Files that hit a transient Windows file lock are deferred to a polling queue: we log a
/// Cargo-style `Blocking waiting for file lock on: ...` line, sleep [`POLL_INTERVAL`], and retry,
/// for up to [`MAX_WAIT`]. Non-lock failures abort immediately.
pub async fn mount_all(mounts: Vec<VirtualDir>, concurrency: usize) -> Result<(), MdtError> {
    let concurrency = concurrency.max(1);
    let mut pending = mounts;
    let mut elapsed = Duration::ZERO;

    while !pending.is_empty() {
        // 1. Attempt every pending mount concurrently; keep the mount alongside its outcome so a
        //    failed one can be re-queued.
        let outcomes = stream::iter(pending.drain(..).map(|mount| async move {
            let outcome = mount_file(mount.source(), mount.target()).await;
            (mount, outcome)
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

        // 2. Separate successes from transient lock failures. A permanent failure aborts now.
        let mut locked = Vec::new();
        for (mount, outcome) in outcomes {
            match outcome {
                MountAttempt::Mounted => {}
                MountAttempt::Locked => locked.push(mount),
                MountAttempt::Failed(e) => return Err(e),
            }
        }

        // 3. Everything mounted.
        if locked.is_empty() {
            return Ok(());
        }

        // 4. Out of patience.
        if elapsed >= MAX_WAIT {
            return Err(MdtError::VirtualDirMountFailed(format!(
                "timed out after {}s waiting for file lock on: {}",
                MAX_WAIT.as_secs(),
                blocked_names(&locked)
            )));
        }

        // 5. Cargo-style blocking message, then wait and retry only the files still locked.
        eprintln!(
            "[mdt] Blocking waiting for file lock on: {} ({} file{}, retrying in {}s...)",
            blocked_names(&locked),
            locked.len(),
            if locked.len() == 1 { "" } else { "s" },
            POLL_INTERVAL.as_secs(),
        );

        tokio::time::sleep(POLL_INTERVAL).await;
        elapsed += POLL_INTERVAL;
        pending = locked;
    }

    Ok(())
}

/// Try to mount `source` at `target` once, preferring the cheapest link strategy.
///
/// Returns [`MountAttempt::Locked`] rather than a hard error when a transient Windows file lock
/// blocks progress, so [`mount_all`] can defer it to a polling queue. A locked target is left
/// untouched and reported for a later attempt — we never delete into a lock.
async fn mount_file(source: &Path, target: &Path) -> MountAttempt {
    // The target's parent directory must exist before we link into it.
    if let Some(parent) = target.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return MountAttempt::Failed(MdtError::VirtualDirMountFailed(format!(
                "cannot create parent directory {}: {e}",
                parent.display()
            )));
        }
    }

    // A symlink already pointing at the source is done — leave it alone (idempotent re-mount).
    if let Ok(link) = tokio::fs::read_link(target).await {
        if link == source {
            return MountAttempt::Mounted;
        }
    }

    // Replace a stale target (a physical file, a wrong symlink, or a broken symlink). Removing a
    // hardlink/symlink never touches the registry source. A locked target is left in place for a
    // later attempt instead of being deleted.
    match tokio::fs::remove_file(target).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if is_lock_error(&e) => return MountAttempt::Locked,
        Err(e) => {
            return MountAttempt::Failed(MdtError::VirtualDirMountFailed(format!(
                "cannot replace existing target {}: {e}",
                target.display()
            )));
        }
    }

    // 1. Hardlink — instant, zero extra space; only works within one volume.
    match tokio::fs::hard_link(source, target).await {
        Ok(()) => return MountAttempt::Mounted,
        Err(e) if is_lock_error(&e) => return MountAttempt::Locked,
        Err(_) => {} // fall through to the next strategy
    }

    // 2. Symlink — works cross-volume on Unix, needs privileges on Windows.
    match symlink_file(source, target).await {
        Ok(()) => return MountAttempt::Mounted,
        Err(e) if is_lock_error(&e) => return MountAttempt::Locked,
        Err(_) => {} // fall through to copy
    }

    // 3. Physical copy — cross-drive Windows, FAT32, or no symlink rights.
    eprintln!(
        "[mdt] WARN: Hardlink/Symlink failed. Falling back to physical copy for {}.",
        target.display()
    );
    match tokio::fs::copy(source, target).await {
        Ok(_) => MountAttempt::Mounted,
        Err(e) if is_lock_error(&e) => MountAttempt::Locked,
        Err(e) => MountAttempt::Failed(MdtError::VirtualDirMountFailed(format!(
            "cannot copy {} to {}: {e}",
            source.display(),
            target.display()
        ))),
    }
}

/// Whether an I/O error is a transient file lock we should wait out.
///
/// On Windows a file held open by another process (most often Windows Defender mid-scan) surfaces
/// as `ERROR_SHARING_VIOLATION` (32) or `ERROR_ACCESS_DENIED` (5). `ERROR_PRIVILEGE_NOT_HELD`
/// (1314, the "no symlink permission" case) is deliberately *not* a lock — it must fall through to
/// the copy strategy instead. On non-Windows there is no equivalent sharing violation, so this is
/// always `false`.
fn is_lock_error(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(32) | Some(5))
}

/// Format the first [`MAX_NAMES_SHOWN`] blocked target names, comma-separated, for a log line.
fn blocked_names(locked: &[VirtualDir]) -> String {
    let mut names = locked
        .iter()
        .take(MAX_NAMES_SHOWN)
        .map(|m| m.target().display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if locked.len() > MAX_NAMES_SHOWN {
        names.push_str(", ...");
    }
    names
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

    #[tokio::test]
    async fn mount_all_mounts_a_batch() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let mounts: Vec<VirtualDir> = (0..5)
            .map(|i| {
                let src = dir.join(format!("src-{i}.bin"));
                let dst = dir.join(format!("dst-{i}.bin"));
                std::fs::write(&src, format!("{i}")).unwrap();
                VirtualDir::new(&src, &dst)
            })
            .collect();

        mount_all(mounts, 2).await.unwrap();

        for i in 0..5 {
            assert_eq!(std::fs::read(dir.join(format!("dst-{i}.bin"))).unwrap(), format!("{i}").into_bytes());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_lock_error_detects_windows_lock_codes() {
        let sharing = std::io::Error::from_raw_os_error(32);
        let access_denied = std::io::Error::from_raw_os_error(5);
        let privilege_not_held = std::io::Error::from_raw_os_error(1314);

        #[cfg(windows)]
        {
            assert!(is_lock_error(&sharing));
            assert!(is_lock_error(&access_denied));
            assert!(!is_lock_error(&privilege_not_held));
        }
        #[cfg(not(windows))]
        {
            assert!(!is_lock_error(&sharing));
            assert!(!is_lock_error(&access_denied));
            assert!(!is_lock_error(&privilege_not_held));
        }
    }

    #[test]
    fn blocked_names_truncates_to_three() {
        let mk = |n: &str| VirtualDir::new("src", std::path::Path::new("dst").join(n));
        let one = vec![mk("a.jar")];
        assert_eq!(
            blocked_names(&one),
            std::path::Path::new("dst").join("a.jar").display().to_string()
        );

        let many = vec![mk("a.jar"), mk("b.jar"), mk("c.jar"), mk("d.jar")];
        let mut expected = ["a.jar", "b.jar", "c.jar"]
            .iter()
            .map(|n| std::path::Path::new("dst").join(n).display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        expected.push_str(", ...");
        assert_eq!(blocked_names(&many), expected);
    }
}
