//! Revert an installed mod loader back to pure vanilla.
//!
//! The seven-step flow:
//!
//! 1. Read the on-disk version JSON and identify the base Minecraft version (`id` field).
//! 2. Fetch the official base manifest (raw text is kept for step 6).
//! 3. Preload every SHA-1 / file name / URL into an in-memory table (no N+1 network queries).
//! 4. Multithreaded scan with MCR markers (Modified / Created / Removed).
//! 5. Execute: re-download modified files, download missing files, delete removed files.
//! 6. Rebuild the base Minecraft JSON by overwriting it with the original vanilla JSON.
//! 7. Re-verify the whole directory against the in-memory table.
//!
//! The whole flow is idempotent: re-running it against an already-vanilla directory is a no-op.

use std::path::Path;

use crate::customize::{Downloader, ProgressReporter};
use crate::downloader::{self, DownloadTask};
use crate::error::MdtError;
use crate::mirror::Mirror;
use crate::modloader::ModLoader;
use crate::urls::ManifestUrls;

use super::mcr::{self, ScanResult};
use super::{read_vanilla_version_id, version_json_path};

/// Revert `vanilla_dir` back to a pure vanilla install.
///
/// `loader` names the loader being removed for error context; the revert itself is driven entirely
/// by the vanilla-manifest comparison, so it is safe to call repeatedly.
pub async fn uninstall_modloader(
    downloader: &dyn Downloader,
    root: &Path,
    vanilla_dir: &Path,
    loader: ModLoader,
    mirror: &Mirror,
    concurrency: usize,
    reporter: &dyn ProgressReporter,
) -> Result<(), MdtError> {
    // 1. Identify the base Minecraft version from the on-disk version JSON `id` field.
    let mc_version = read_vanilla_version_id(vanilla_dir).await?;

    // 2. Fetch the base manifest; the raw text is kept for step 6.
    let (vanilla_text, version_json) =
        crate::manifest::fetch_version_json(downloader, mirror, &mc_version).await?;

    // 3. Pre-load the official SHA-1 / file name / URL table into memory.
    let manifest = mcr::build_manifest(&version_json, root);

    // 4. Multi-threaded scan → MCR markers.
    let vanilla_json_path = version_json_path(vanilla_dir)?;
    let marker_dir = vanilla_dir.join(".mdt-mcr");
    let _ = tokio::fs::remove_dir_all(&marker_dir).await;

    let manifest_for_scan = manifest.clone();
    let scan_dir = vanilla_dir.to_path_buf();
    let scan_json = vanilla_json_path.clone();
    let scan = tokio::task::spawn_blocking(move || {
        mcr::scan_manifest(&manifest_for_scan, &scan_dir, &scan_json)
    })
    .await??;

    mcr::write_markers(&scan, &marker_dir).await?;

    // 5. Execute: restore modified, download created, delete removed.
    execute_restore(downloader, mirror, concurrency, &scan, reporter).await?;

    // 6. Rebuild the base Minecraft JSON.
    tokio::fs::write(&vanilla_json_path, &vanilla_text).await?;

    // 7. Full verification against the in-memory manifest table.
    let manifest_for_verify = manifest;
    tokio::task::spawn_blocking(move || mcr::verify_manifest(&manifest_for_verify))
        .await?
        .map_err(|e| e.context(format!("uninstalling {}", loader.display_name())))?;

    // Success: drop the marker files now that the revert is complete.
    let _ = tokio::fs::remove_dir_all(&marker_dir).await;

    Ok(())
}

/// Download/delete the files the scan flagged, reusing the SHA-1 verified downloader.
async fn execute_restore(
    downloader: &dyn Downloader,
    mirror: &Mirror,
    concurrency: usize,
    scan: &ScanResult,
    reporter: &dyn ProgressReporter,
) -> Result<(), MdtError> {
    let urls = ManifestUrls::try_new(mirror)?;

    let tasks: Vec<DownloadTask> = scan
        .modified
        .iter()
        .chain(scan.created.iter())
        .map(|entry| DownloadTask {
            url: entry.url.clone(),
            dest_path: entry.path.clone(),
            sha1: entry.sha1.clone(),
            size: entry.size,
            extract_to: None,
            label: entry.path.file_name().and_then(|n| n.to_str()).map(str::to_string),
            by: None,
        })
        .collect();

    if !tasks.is_empty() {
        downloader::execute_downloads(downloader, tasks, concurrency, mirror, &urls, reporter).await?;
    }

    for path in &scan.removed {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(MdtError::IoError(e).context(format!("removing {}", path.display())));
            }
        }
    }

    Ok(())
}
