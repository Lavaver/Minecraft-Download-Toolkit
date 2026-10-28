//! Dual-loader (Fabric + Quilt) state detection and context switching.
//!
//! A dual-loader version directory keeps three JSON files alongside the active one:
//!
//! * `<mc_version>.json.mcvanila` — the pristine vanilla backup (uninstall reference).
//! * `<mc_version>+Fabric<ver>.json` — the Fabric configuration.
//! * `<mc_version>+Quilt<ver>.json` — the Quilt configuration.
//! * `<mc_version>.json` — the *active* configuration (a copy of one of the two above).
//!
//! Switching context means copying the inactive loader's JSON over `<mc_version>.json`. Since
//! Quilt is a fork of Fabric and loads Fabric mods, a "dual loader" instance is really Quilt doing
//! the loading while both configurations are kept on disk so the launcher can flip between them.
//!
//! # Detection
//!
//! A version JSON is classified by its *contents*, not its name: the `mainClass` (launch entry
//! point) is the authoritative signal, with `libraries` as the fallback. The `id` field is ignored
//! — it just mirrors the filename and changes when a player renames the folder.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::MdtError;
use crate::modloader::ModLoader;

/// The dual-loader state of a version directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DualLoaderState {
    /// Pure vanilla: no `.mcvanila` backup and no loader JSONs.
    Vanilla,
    /// Exactly one of Fabric/Quilt is installed (`present`).
    Single { present: ModLoader },
    /// Both Fabric and Quilt configurations are present.
    Dual,
}

/// The result of [`crate::MdtClient::setup_dual_loader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DualLoaderInfo {
    /// The Minecraft version this directory belongs to (its directory name).
    pub mc_version: String,
    /// The Fabric loader version now installed, if present.
    pub fabric_version: Option<String>,
    /// The Quilt loader version now installed, if present.
    pub quilt_version: Option<String>,
    /// The loader whose configuration is now active (`<mc_version>.json`).
    pub active: ModLoader,
}

/// Classify a parsed version JSON as Fabric / Quilt / neither.
///
/// `mainClass` is the primary signal:
///
/// * Fabric — `net.fabricmc…` (e.g. `net.fabricmc.loader.impl.launch.knot.KnotClient`)
/// * Quilt  — `org.quiltmc…` (e.g. `org.quiltmc.loader.impl.launch.knot.KnotClient`)
/// * Vanilla — `net.minecraft…` (or anything else) → `None`
///
/// When `mainClass` is missing or unhelpful, `libraries` is consulted for the loader's own
/// dependency coordinate (`net.fabricmc:fabric-loader` / `org.quiltmc:quilt-loader`). The `id`
/// field is deliberately ignored.
pub fn loader_from_json_value(value: &serde_json::Value) -> Option<ModLoader> {
    if let Some(main) = value.get("mainClass").and_then(|v| v.as_str()) {
        if main.contains("net.fabricmc") {
            return Some(ModLoader::Fabric);
        }
        if main.contains("org.quiltmc") {
            return Some(ModLoader::Quilt);
        }
    }
    for lib in value.get("libraries").and_then(|v| v.as_array())?.iter() {
        let Some(name) = lib.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name.starts_with("net.fabricmc:fabric-loader") {
            return Some(ModLoader::Fabric);
        }
        if name.starts_with("org.quiltmc:quilt-loader") {
            return Some(ModLoader::Quilt);
        }
    }
    None
}

/// The loader version declared by a parsed version JSON's `libraries`, if it declares `loader`.
pub fn loader_version_from_json_value(
    value: &serde_json::Value,
    loader: ModLoader,
) -> Option<String> {
    let prefix = match loader {
        ModLoader::Fabric => "net.fabricmc:fabric-loader:",
        ModLoader::Quilt => "org.quiltmc:quilt-loader:",
        _ => return None,
    };
    for lib in value.get("libraries")?.as_array()? {
        let name = lib.get("name")?.as_str()?;
        if let Some(rest) = name.strip_prefix(prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Read and parse a version JSON file, or `None` on any error.
fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Classify a version JSON file by its contents (never its filename or `id`).
pub fn loader_from_json(path: &Path) -> Option<ModLoader> {
    loader_from_json_value(&read_json(path)?)
}

/// The loader version declared by a version JSON file's `libraries`.
pub fn loader_version_from_json(path: &Path, loader: ModLoader) -> Option<String> {
    loader_version_from_json_value(&read_json(path)?, loader)
}

/// Detect the dual-loader state of `version_dir`.
///
/// Every `<name>.json`'s contents are classified via [`loader_from_json`]; the `.mcvanila` backup
/// and `.jar` files are skipped because they don't end in `.json`.
pub fn detect_dual_loader_state(version_dir: &Path) -> DualLoaderState {
    let mut has_fabric = false;
    let mut has_quilt = false;

    for entry in std::fs::read_dir(version_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        match loader_from_json(&path) {
            Some(ModLoader::Fabric) => has_fabric = true,
            Some(ModLoader::Quilt) => has_quilt = true,
            _ => {}
        }
    }

    match (has_fabric, has_quilt) {
        (true, true) => DualLoaderState::Dual,
        (true, false) => DualLoaderState::Single { present: ModLoader::Fabric },
        (false, true) => DualLoaderState::Single { present: ModLoader::Quilt },
        (false, false) => DualLoaderState::Vanilla,
    }
}

/// The loader currently active in `<mc_version>.json`, if it is Fabric or Quilt.
pub fn active_loader(version_dir: &Path, mc_version: &str) -> Option<ModLoader> {
    loader_from_json(&version_dir.join(format!("{mc_version}.json")))
}

/// The installed version of `loader` in `version_dir`, if present.
pub fn installed_loader_version(
    version_dir: &Path,
    mc_version: &str,
    loader: ModLoader,
) -> Option<String> {
    let path = find_loader_json(version_dir, mc_version, loader)?;
    loader_version_from_json(&path, loader)
}

/// The opposite of a Fabric/Quilt loader (used for context switching).
#[must_use]
pub fn opposite(loader: ModLoader) -> ModLoader {
    match loader {
        ModLoader::Fabric => ModLoader::Quilt,
        ModLoader::Quilt => ModLoader::Fabric,
        other => other,
    }
}

/// Find the `<mc_version>+<Loader><ver>.json` file for `loader`, if it exists.
pub fn find_loader_json(
    version_dir: &Path,
    mc_version: &str,
    loader: ModLoader,
) -> Option<PathBuf> {
    let prefix = format!("{mc_version}+{}", loader.display_name());
    for entry in std::fs::read_dir(version_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".json") {
            return Some(entry.path());
        }
    }
    None
}

/// How many times to retry a copy that keeps hitting a transient Windows sharing violation.
const COPY_LOCK_RETRIES: u32 = 3;

/// Delay between sharing-violation retries.
const COPY_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Whether an I/O error is a transient Windows sharing violation (raw OS error 32) — the file is
/// briefly locked by another process (Windows Defender, a running launcher, ...).
///
/// Error 32 is checked via `raw_os_error` rather than `ErrorKind` because Rust maps it to
/// `Uncategorized` (not `Other`/`PermissionDenied`) on Windows.
fn is_sharing_violation(e: &std::io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(32)
}

/// `tokio::fs::copy` with a bounded retry on transient Windows file locks (sharing violation).
async fn copy_with_retry(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let mut attempts = 0;
    loop {
        return match tokio::fs::copy(src, dst).await {
            Ok(n) => Ok(n),
            Err(e) => {
                if attempts < COPY_LOCK_RETRIES && is_sharing_violation(&e) {
                    attempts += 1;
                    tokio::time::sleep(COPY_RETRY_BACKOFF).await;
                    continue;
                }
                Err(e)
            }
        }
    }
}

/// Switch the active loader context of `version_dir` to `target_loader`.
///
/// Copies `<mc_version>+<TargetLoader><ver>.json` over `<mc_version>.json` so the launcher uses
/// the target loader. The `.mcvanila` backup and both loader JSONs are left untouched.
///
/// The copy retries a few times on a transient Windows sharing violation (error 32), since the
/// active JSON is commonly held by Windows Defender or a running launcher.
///
/// The target loader's configuration must already exist on disk (i.e. `target_loader` must be
/// installed); otherwise an [`MdtError::InvalidManifest`] is returned.
pub async fn switch_loader_context(
    version_dir: &Path,
    target_loader: ModLoader,
) -> Result<(), MdtError> {
    let mc_version = version_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            MdtError::InvalidManifest(format!(
                "version directory has no valid name: {}",
                version_dir.display()
            ))
        })?;

    let target_json = find_loader_json(version_dir, mc_version, target_loader).ok_or_else(|| {
        MdtError::InvalidManifest(format!(
            "target loader {} is not installed in {}: cannot switch context. \
             Call `setup_dual_loader()` first to install it",
            target_loader.display_name(),
            version_dir.display()
        ))
    })?;

    let active = version_dir.join(format!("{mc_version}.json"));
    copy_with_retry(&target_json, &active)
        .await
        .map_err(|e| {
            MdtError::IoError(e).context(format!(
                "switching loader context to {}",
                target_loader.display_name()
            ))
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mdt-dual-{}-{n}", std::process::id()))
    }

    #[test]
    fn detect_vanilla_state() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.16.5.json"),
            r#"{"id":"1.16.5","mainClass":"net.minecraft.client.main.Main","libraries":[{"name":"com.mojang:client:1.16.5"}]}"#,
        )
        .unwrap();

        assert_eq!(detect_dual_loader_state(&dir), DualLoaderState::Vanilla);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_single_loader_from_active_json() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.16.5.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","inheritsFrom":"1.16.5","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();

        assert_eq!(
            detect_dual_loader_state(&dir),
            DualLoaderState::Single { present: ModLoader::Fabric }
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_reads_mainclass_not_id() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // The `id` lies and the filename is non-standard, but `mainClass` tells the truth.
        std::fs::write(
            dir.join("weird-name.json"),
            r#"{"id":"1.16.5","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();

        assert_eq!(
            detect_dual_loader_state(&dir),
            DualLoaderState::Single { present: ModLoader::Fabric }
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_dual_state() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.16.5.json"),
            r#"{"id":"1.16.5+Quilt0.24.0","inheritsFrom":"1.16.5","mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"org.quiltmc:quilt-loader:0.24.0"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("1.16.5+Fabric0.20.0.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("1.16.5+Quilt0.24.0.json"),
            r#"{"id":"1.16.5+Quilt0.24.0","mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"org.quiltmc:quilt-loader:0.24.0"}]}"#,
        )
        .unwrap();

        assert_eq!(detect_dual_loader_state(&dir), DualLoaderState::Dual);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installed_version_reads_from_libraries() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.16.5+Fabric0.20.0.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();

        assert_eq!(
            installed_loader_version(&dir, "1.16.5", ModLoader::Fabric),
            Some("0.20.0".to_string())
        );
        assert_eq!(installed_loader_version(&dir, "1.16.5", ModLoader::Quilt), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loader_from_json_value_classifies_by_mainclass_and_libraries() {
        let fabric: serde_json::Value = serde_json::from_str(
            r#"{"mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient"}"#,
        )
        .unwrap();
        let quilt: serde_json::Value = serde_json::from_str(
            r#"{"mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient"}"#,
        )
        .unwrap();
        let vanilla: serde_json::Value = serde_json::from_str(
            r#"{"mainClass":"net.minecraft.client.main.Main"}"#,
        )
        .unwrap();
        // mainClass missing, but libraries reveal the loader.
        let fabric_by_libs: serde_json::Value = serde_json::from_str(
            r#"{"libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();

        assert_eq!(loader_from_json_value(&fabric), Some(ModLoader::Fabric));
        assert_eq!(loader_from_json_value(&quilt), Some(ModLoader::Quilt));
        assert_eq!(loader_from_json_value(&vanilla), None);
        assert_eq!(loader_from_json_value(&fabric_by_libs), Some(ModLoader::Fabric));

        assert_eq!(opposite(ModLoader::Fabric), ModLoader::Quilt);
        assert_eq!(opposite(ModLoader::Quilt), ModLoader::Fabric);
    }

    #[tokio::test]
    async fn switch_loader_context_swaps_to_fabric() {
        let base = temp_dir();
        let dir = base.join("1.16.5");
        std::fs::create_dir_all(&dir).unwrap();
        // Quilt is currently active.
        std::fs::write(
            dir.join("1.16.5.json"),
            r#"{"id":"1.16.5+Quilt0.24.0","mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"org.quiltmc:quilt-loader:0.24.0"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("1.16.5+Fabric0.20.0.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("1.16.5+Quilt0.24.0.json"),
            r#"{"id":"1.16.5+Quilt0.24.0","mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"org.quiltmc:quilt-loader:0.24.0"}]}"#,
        )
        .unwrap();

        switch_loader_context(&dir, ModLoader::Fabric).await.unwrap();

        let active: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("1.16.5.json")).unwrap()).unwrap();
        assert_eq!(
            active["mainClass"],
            "net.fabricmc.loader.impl.launch.knot.KnotClient"
        );
        assert!(active["libraries"][0]["name"]
            .as_str()
            .unwrap()
            .starts_with("net.fabricmc:fabric-loader"));

        // Both loader configs are preserved on disk.
        assert!(dir.join("1.16.5+Fabric0.20.0.json").exists());
        assert!(dir.join("1.16.5+Quilt0.24.0.json").exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn switch_loader_context_errors_when_target_missing() {
        let base = temp_dir();
        let dir = base.join("1.16.5");
        std::fs::create_dir_all(&dir).unwrap();
        // Only Fabric is installed.
        std::fs::write(
            dir.join("1.16.5.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("1.16.5+Fabric0.20.0.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();

        let err = switch_loader_context(&dir, ModLoader::Quilt).await.unwrap_err();
        assert!(matches!(err, MdtError::InvalidManifest(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("Quilt") && msg.contains("setup_dual_loader"), "got {msg}");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn is_sharing_violation_detects_windows_error_32() {
        let e = std::io::Error::from_raw_os_error(32);
        #[cfg(windows)]
        assert!(is_sharing_violation(&e));
        #[cfg(not(windows))]
        assert!(!is_sharing_violation(&e));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn switch_loader_context_retries_then_errors_on_locked_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let base = temp_dir();
        let dir = base.join("1.16.5");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.16.5.json"),
            r#"{"id":"1.16.5+Quilt0.24.0","mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"org.quiltmc:quilt-loader:0.24.0"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("1.16.5+Fabric0.20.0.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.20.0"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("1.16.5+Quilt0.24.0.json"),
            r#"{"id":"1.16.5+Quilt0.24.0","mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"org.quiltmc:quilt-loader:0.24.0"}]}"#,
        )
        .unwrap();

        // Hold the active JSON open without sharing, so the copy cannot write over it. This is the
        // Windows equivalent of Defender/a launcher briefly locking the file (sharing violation).
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(dir.join("1.16.5.json"))
            .unwrap();

        // The retry loop should exhaust its attempts and surface a context-wrapped I/O error.
        let err = switch_loader_context(&dir, ModLoader::Fabric).await.unwrap_err();
        match err {
            MdtError::Context { source, .. } => assert!(matches!(*source, MdtError::IoError(_))),
            other => panic!("expected a context-wrapped I/O error, got {other:?}"),
        }

        drop(lock);
        std::fs::remove_dir_all(&base).ok();
    }
}
