//! MCR (Modified, Created, Removed) markers and the multithreaded SHA-1 scanner.
//!
//! Uninstalling a mod loader is a SHA-1 manifest comparison. The vanilla manifest tells us every
//! file that *should* exist and its official hash; the disk tells us what *actually* exists. The
//! scanner classifies every difference as one of:
//!
//! * **Modified** — file exists but its SHA-1 no longer matches the manifest.
//! * **Created** — file is missing from disk entirely.
//! * **Removed** — file is on disk but not in the vanilla manifest (loader droppings).
//!
//! # Scanner strategy
//!
//! The parallel pass is a hybrid bucket-sort + quicksort-inspired partition. Because the manifest
//! already carries every file's official hash, we bucket by the hash's leading nibble (bucket sort)
//! and then sort each bucket by path so adjacent files are hashed back-to-back (improved disk
//! locality, a quicksort-style partition). Buckets are handed to a *bounded* pool of `scoped`
//! worker threads; each worker owns its results, so no lock is ever taken. The in-memory manifest
//! table is shared read-only, so it needs no `Mutex`.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::downloader::{self, VersionJson};
use crate::error::MdtError;

/// How many hash-prefix buckets to partition the manifest into (one per hex nibble).
const HASH_BUCKETS: usize = 16;

/// Upper bound on scanner worker threads, on top of the OS's reported parallelism.
const MAX_WORKERS: usize = 8;

/// A single file from the vanilla manifest, with everything needed to restore it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// Absolute on-disk path (under the `.minecraft` root).
    pub path: PathBuf,
    /// Official SHA-1 from the manifest.
    pub sha1: String,
    /// Download URL, for restoring a modified or missing file.
    pub url: String,
    /// Expected byte size (used to fast-skip unchanged files).
    pub size: u64,
}

/// The result of one scan pass: every file classified as Modified, Created, or Removed.
#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub modified: Vec<ManifestFile>,
    pub created: Vec<ManifestFile>,
    pub removed: Vec<PathBuf>,
}

/// Build the in-memory manifest table from a parsed vanilla `version.json`.
///
/// Reuses the same download-task generator as [`crate::MdtClient::install_vanilla`] so install and
/// uninstall agree on exactly which files belong to a version (client JAR, libraries, asset index).
pub fn build_manifest(version_json: &VersionJson, root: &Path) -> Vec<ManifestFile> {
    downloader::build_tasks_from_version_json(version_json, root)
        .into_iter()
        .map(|task| ManifestFile {
            path: task.dest_path,
            sha1: task.sha1,
            url: task.url,
            size: task.size,
        })
        .collect()
}

/// Scan the manifest files on disk, multi-threaded, and classify every difference.
///
/// `version_json` is the on-disk vanilla JSON path; it is never flagged as *Removed* (the JSON is
/// not part of the download manifest, but it is vanilla, not a loader dropping).
pub fn scan_manifest(
    manifest: &[ManifestFile],
    vanilla_dir: &Path,
    version_json: &Path,
) -> Result<ScanResult, MdtError> {
    let mut result = ScanResult::default();

    if !manifest.is_empty() {
        let buckets = bucket_by_hash_prefix(manifest);
        let workers = worker_count(manifest.len());
        let mut queues: Vec<Vec<ManifestFile>> = (0..workers).map(|_| Vec::new()).collect();
        for (i, bucket) in buckets.into_iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            queues[i % workers].extend(bucket);
        }

        let partial = std::thread::scope(|scope| -> Result<ScanResult, MdtError> {
            let handles: Vec<_> = queues
                .into_iter()
                .filter(|q| !q.is_empty())
                .map(|queue| scope.spawn(move || hash_worker(&queue)))
                .collect();

            let mut combined = ScanResult::default();
            for handle in handles {
                let part = handle
                    .join()
                    .map_err(|_| MdtError::UninstallFailed("scanner worker panicked".to_string()))??;
                combined.modified.extend(part.modified);
                combined.created.extend(part.created);
            }
            Ok(combined)
        })?;

        result.modified = partial.modified;
        result.created = partial.created;
    }

    result.removed = find_removed_files(manifest, vanilla_dir, version_json)?;
    Ok(result)
}

/// Walk `vanilla_dir` and flag every regular file not present in the manifest (or the vanilla
/// JSON) as *Removed*. `mods/`, extracted natives, marker files, and MDT scratch dirs are skipped.
pub fn find_removed_files(
    manifest: &[ManifestFile],
    vanilla_dir: &Path,
    version_json: &Path,
) -> Result<Vec<PathBuf>, MdtError> {
    let mut known: HashSet<PathBuf> = manifest.iter().map(|m| m.path.clone()).collect();
    known.insert(version_json.to_path_buf());

    let mut removed = Vec::new();
    collect_removed(vanilla_dir, &known, &mut removed)?;
    Ok(removed)
}

fn collect_removed(
    dir: &Path,
    known: &HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) -> Result<(), MdtError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if entry.file_type()?.is_dir() {
            // Preserve `mods/` (vanilla never loads it; keep it for a future re-install), the
            // runtime-extracted natives dir, and MDT's own marker/scratch dirs.
            if name == "mods" || name.ends_with("-natives") || name.starts_with(".mdt-") {
                continue;
            }
            collect_removed(&path, known, out)?;
        } else if entry.file_type()?.is_file() {
            if is_marker_file(&name) || known.contains(&path) {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

/// A file name that marks an MCR marker (either from this run or a previous, interrupted one).
fn is_marker_file(name: &str) -> bool {
    name.ends_with(".pinned") || name.ends_with(".pinned.add") || name.ends_with(".pinned.remove")
}

/// Write the MCR markers to disk as a durable record of the scan result.
///
/// Naming follows the convention `[Hash].pinned` (modified), `[Hash].pinned.add` (created) and
/// `[Filename].pinned.remove` (removed). Execution is driven from the in-memory [`ScanResult`];
/// these files exist so an interrupted revert can be inspected (and re-run safely).
pub async fn write_markers(scan: &ScanResult, marker_dir: &Path) -> Result<(), MdtError> {
    tokio::fs::create_dir_all(marker_dir).await?;
    for m in &scan.modified {
        tokio::fs::write(marker_dir.join(format!("{}.pinned", m.sha1)), b"").await?;
    }
    for m in &scan.created {
        tokio::fs::write(marker_dir.join(format!("{}.pinned.add", m.sha1)), b"").await?;
    }
    for path in &scan.removed {
        let base = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("removed");
        tokio::fs::write(marker_dir.join(format!("{base}.pinned.remove")), b"").await?;
    }
    Ok(())
}

/// Re-verify every manifest file after a revert. Returns the offending path on the first failure.
pub fn verify_manifest(manifest: &[ManifestFile]) -> Result<(), MdtError> {
    for entry in manifest {
        match sha1_of_file(&entry.path)? {
            Some(actual) if actual == entry.sha1 => {}
            Some(actual) => {
                return Err(MdtError::ChecksumMismatch {
                    path: entry.path.display().to_string(),
                    expected: entry.sha1.clone(),
                    actual,
                });
            }
            None => {
                return Err(MdtError::UninstallFailed(format!(
                    "verification failed: {} is still missing",
                    entry.path.display()
                )));
            }
        }
    }
    Ok(())
}

// ===== internals =====

/// Partition the manifest into hash-prefix buckets (bucket sort), then sort each bucket by path.
fn bucket_by_hash_prefix(manifest: &[ManifestFile]) -> Vec<Vec<ManifestFile>> {
    let mut buckets: Vec<Vec<ManifestFile>> = (0..HASH_BUCKETS).map(|_| Vec::new()).collect();
    for entry in manifest {
        buckets[hash_prefix_index(&entry.sha1)].push(entry.clone());
    }
    for bucket in &mut buckets {
        // Locality sort: adjacent paths are read together, reducing disk seeks.
        bucket.sort_by(|a, b| a.path.cmp(&b.path));
    }
    buckets
}

fn hash_prefix_index(sha1: &str) -> usize {
    match sha1.as_bytes().first().copied() {
        Some(b) if b.is_ascii_hexdigit() => (b as char).to_digit(16).unwrap_or(0) as usize,
        _ => 0,
    }
}

fn worker_count(len: usize) -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cpus.clamp(1, MAX_WORKERS).min(len)
}

/// Hash a batch of manifest files, classifying each as unchanged, Modified, or Created.
fn hash_worker(files: &[ManifestFile]) -> Result<ScanResult, MdtError> {
    let mut result = ScanResult::default();
    for entry in files {
        match sha1_of_file(&entry.path)? {
            Some(actual) if actual == entry.sha1 => {}
            Some(_) => result.modified.push(entry.clone()),
            None => result.created.push(entry.clone()),
        }
    }
    Ok(result)
}

/// Compute a file's SHA-1 synchronously, or `Ok(None)` if it does not exist.
fn sha1_of_file(path: &Path) -> Result<Option<String>, MdtError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(MdtError::IoError(e).context(format!("opening {}", path.display()))),
    };

    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

// ===== tests =====

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mdt-mcr-{tag}-{}-{n}", std::process::id()))
    }

    fn sha1_hex(content: &[u8]) -> String {
        hex::encode(Sha1::digest(content))
    }

    fn entry(path: &Path, content: &[u8]) -> ManifestFile {
        ManifestFile {
            path: path.to_path_buf(),
            sha1: sha1_hex(content),
            url: "https://example.invalid/file".to_string(),
            size: content.len() as u64,
        }
    }

    #[test]
    fn scan_classifies_modified_created_and_unchanged() {
        let dir = unique_dir("scan");
        fs::create_dir_all(&dir).unwrap();

        let ok_path = dir.join("ok.bin");
        let modified_path = dir.join("modified.bin");
        let missing_path = dir.join("missing.bin");

        fs::write(&ok_path, b"correct").unwrap();
        fs::write(&modified_path, b"tampered").unwrap();

        let manifest = vec![
            entry(&ok_path, b"correct"),
            entry(&modified_path, b"original"),
            entry(&missing_path, b"original"),
        ];

        let version_json = dir.join("1.0.json");
        fs::write(&version_json, b"{}").unwrap();

        let scan = scan_manifest(&manifest, &dir, &version_json).unwrap();

        assert_eq!(scan.modified.len(), 1);
        assert_eq!(scan.modified[0].path, modified_path);
        assert_eq!(scan.created.len(), 1);
        assert_eq!(scan.created[0].path, missing_path);
        // The version JSON itself is kept (not Removed), and the three manifest files are known.
        assert!(scan.removed.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removed_scan_flags_extra_files_but_preserves_mods_and_markers() {
        let dir = unique_dir("removed");
        let mods = dir.join("mods");
        let marker_dir = dir.join(".mdt-mcr");
        fs::create_dir_all(&mods).unwrap();
        fs::create_dir_all(&marker_dir).unwrap();

        let extra = dir.join("extra.json");
        let keep = dir.join("keep.bin");
        fs::write(&extra, b"x").unwrap();
        fs::write(&keep, b"y").unwrap();
        fs::write(mods.join("my-mod.jar"), b"z").unwrap();
        fs::write(marker_dir.join("deadbeef.pinned"), b"").unwrap();

        let version_json = dir.join("1.0.json");
        fs::write(&version_json, b"{}").unwrap();

        let manifest = vec![entry(&keep, b"y")];
        let removed = find_removed_files(&manifest, &dir, &version_json).unwrap();

        assert_eq!(removed, vec![extra]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verify_manifest_fails_on_mismatch_and_missing() {
        let dir = unique_dir("verify");
        fs::create_dir_all(&dir).unwrap();

        let bad = dir.join("bad.bin");
        fs::write(&bad, b"tampered").unwrap();
        let manifest = vec![entry(&bad, b"expected")];
        assert!(matches!(verify_manifest(&manifest), Err(MdtError::ChecksumMismatch { .. })));

        let missing = dir.join("missing.bin");
        let manifest = vec![entry(&missing, b"expected")];
        assert!(matches!(verify_manifest(&manifest), Err(MdtError::UninstallFailed(_))));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verify_manifest_passes_when_everything_matches() {
        let dir = unique_dir("verify-ok");
        fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok.bin");
        fs::write(&ok, b"data").unwrap();
        let manifest = vec![entry(&ok, b"data")];
        verify_manifest(&manifest).unwrap();
        fs::remove_dir_all(&dir).ok();
    }
}
