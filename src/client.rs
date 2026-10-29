//! The [`MdtClient`] and its builder.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::asset::AssetRegistry;
use crate::customize::{Downloader, HttpDownloader, ProgressReporter, StderrReporter};
use crate::downloader::{self, VersionJson};
use crate::dual_loader::{self, DualLoaderInfo, DualLoaderState};
use crate::error::MdtError;
use crate::http;
use crate::installer::{self, InstallRequest, InstalledVersion};
use crate::manifest::{self, latest_stable_loader_version, VersionEntry};
use crate::mirror::Mirror;
use crate::modloader::ModLoader;
use crate::urls::ManifestUrls;

/// Default download concurrency.
///
/// Three is enough for most connections; go higher and some mirrors will rate-limit or 403 you.
const DEFAULT_CONCURRENCY: usize = 3;

/// The `MdtClient`. One instance holds everything a download needs: a pooled HTTP client, the
/// active [`Mirror`], and the `.minecraft` root directory. Build one with [`MdtClient::builder`]
/// and reuse it for as many installs as you want.
///
/// ```no_run
/// use mdt::{MdtClient, Mirror, ModLoader};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let client = MdtClient::builder()
///         .mirror(Mirror::BMCLAPI)
///         .root_dir("./.minecraft")
///         .build()?;
///
///     client.install_vanilla("1.20.1").await?;
///
///     let vanilla_dir = client.root_dir().join("versions").join("1.20.1");
///     client.install_modloader(&vanilla_dir, ModLoader::Fabric, "0.15.11", true).await?;
///
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct MdtClient {
    http: reqwest::Client,
    downloader: Arc<dyn Downloader>,
    mirror: Mirror,
    root: PathBuf,
    concurrency: usize,
    reporter: Arc<dyn ProgressReporter>,
    proxy: Option<String>,
    connect_timeout: Duration,
    read_timeout: Duration,
}

impl MdtClient {
    /// Create a new [`MdtClientBuilder`].
    #[must_use]
    pub fn builder() -> MdtClientBuilder {
        MdtClientBuilder::new()
    }

    /// The shared HTTP client (connection-pooled, with a browser User-Agent).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The active transport (files and metadata).
    pub fn downloader(&self) -> &dyn Downloader {
        self.downloader.as_ref()
    }

    /// The active mirror source.
    pub fn mirror(&self) -> &Mirror {
        &self.mirror
    }

    /// The target `.minecraft` root directory.
    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    /// The configured download concurrency.
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// The configured HTTP proxy URL, if any.
    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    /// The configured connect timeout.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// The configured read timeout.
    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    /// Change the download concurrency at runtime (clamped to at least 1).
    ///
    /// Unlike [`MdtClientBuilder::concurrency`], this works after the client is built — handy for
    /// a user preference or a `--jobs`-style CLI flag.
    pub fn set_concurrency(&mut self, concurrency: usize) {
        self.concurrency = concurrency.max(1);
    }

    /// Fetch the list of available Minecraft versions.
    pub async fn versions(&self) -> Result<Vec<VersionEntry>, MdtError> {
        self.reporter.status(&format!("Updating {} index", self.mirror.label()));
        let manifest = manifest::fetch_minecraft_version_manifest(self.downloader.as_ref(), &self.mirror).await?;
        Ok(manifest.versions)
    }

    /// Download and install a vanilla Minecraft version.
    ///
    /// Returns the parsed [`VersionJson`]. Downloads the client JAR, libraries (rules and natives
    /// extraction included), and asset index, and writes the version JSON to
    /// `versions/<id>/<id>.json` so launchers and loader installers can read it back.
    ///
    /// # Errors
    ///
    /// * [`MdtError::VersionNotFound`] if `version` isn't in the manifest.
    /// * [`MdtError::NetworkError`] / [`MdtError::ChecksumMismatch`] on download failures.
    pub async fn install_vanilla(&self, version: &str) -> Result<VersionJson, MdtError> {
        self.reporter.status(&format!("Updating {} index", self.mirror.label()));
        let (text, version_json) =
            manifest::fetch_version_json(self.downloader.as_ref(), &self.mirror, version).await?;
        let urls = ManifestUrls::try_new(&self.mirror)?;

        // Save the vanilla version JSON.
        let vdir = self.root.join("versions").join(&version_json.id);
        tokio::fs::create_dir_all(&vdir).await?;
        tokio::fs::write(vdir.join(format!("{}.json", version_json.id)), &text).await?;

        // Client JAR + libraries (+ asset index). Vanilla stays untouched.
        let tasks = downloader::build_tasks_from_version_json(&version_json, &self.root);
        downloader::execute_downloads(
            self.downloader.as_ref(),
            tasks,
            self.concurrency,
            &self.mirror,
            &urls,
            self.reporter.as_ref(),
        )
        .await?;

        // Assets: fetch the index (via the global index cache), then sync the global registry
        // cache into `assets/objects`.
        if let Some(asset_index) = &version_json.asset_index {
            AssetRegistry::fetch_and_sync(
                &self.root,
                asset_index,
                &version_json.id,
                self.downloader.as_ref(),
                &self.mirror,
                &urls,
                self.reporter.as_ref(),
            )
            .await?;
        }

        Ok(version_json)
    }

    /// Install a mod loader, given an existing vanilla version directory.
    ///
    /// The Minecraft version is read from `<vanilla_dir>/<vanilla_dir-name>.json`'s `id` field,
    /// which is authoritative for compatibility checks.
    ///
    /// When `copy_vanilla` is `true`, `vanilla_dir` is first copied to the isolated
    /// `versions/<mc_version>+<Loader><loader_version>` directory (Method A loaders use their own
    /// `-forge-`/`-neoforge-` convention) and the loader is installed there, leaving vanilla
    /// untouched. When `false`, the loader is installed directly inside `vanilla_dir`, mutating it.
    ///
    /// # Errors
    ///
    /// * [`MdtError::InvalidManifest`] if `vanilla_dir` has no readable version JSON.
    /// * [`MdtError::LoaderInstallFailed`] if the loader doesn't support the version.
    /// * [`MdtError::JavaNotFound`] if a Method A loader is requested but there's no Java.
    pub async fn install_modloader(
        &self,
        vanilla_dir: &Path,
        loader: ModLoader,
        loader_version: &str,
        copy_vanilla: bool,
    ) -> Result<InstalledVersion, MdtError> {
        let mc_version = installer::read_vanilla_version_id(vanilla_dir).await?;
        eprintln!("[mdt] install_modloader: resolved mc_version={mc_version}");

        if !loader.compatible_with(&mc_version) {
            return Err(MdtError::LoaderInstallFailed(format!(
                "{} is not compatible with Minecraft {}",
                loader.display_name(),
                mc_version
            )));
        }

        // Copy-on-write: clone the vanilla directory into the loader's isolated directory before
        // installing, so vanilla is never mutated. Otherwise install in place.
        let target_dir = if copy_vanilla {
            let isolated_id = installer::installer_version_id(&mc_version, loader, loader_version);
            let isolated_dir = installer::version_dir(&self.root, &isolated_id);
            eprintln!(
                "[mdt] install_modloader: copy-on-write {} -> {}",
                vanilla_dir.display(),
                isolated_dir.display()
            );
            installer::copy_dir_recursive(vanilla_dir, &isolated_dir).await?;
            eprintln!("[mdt] install_modloader: copy-on-write done");
            None
        } else {
            Some(vanilla_dir.to_path_buf())
        };

        let req = InstallRequest {
            mc_version,
            loader,
            loader_version: loader_version.to_string(),
            root_dir: self.root.clone(),
            mirror: self.mirror.clone(),
            target_dir,
            reporter: self.reporter.clone(),
        };
        installer::install_modloader(self.downloader.as_ref(), req).await
    }

    /// Install a complete stack — vanilla and then a mod loader — in one call.
    ///
    /// If the vanilla version isn't on disk yet it is downloaded first; otherwise the existing
    /// install is reused. Then the loader is installed with the same copy-on-write behavior as
    /// [`MdtClient::install_modloader`]. This is what the [`full_install!`](crate::full_install)
    /// macro expands to.
    ///
    /// Returns the loader [`InstalledVersion`].
    pub async fn full_install(
        &self,
        version: &str,
        loader: ModLoader,
        loader_version: &str,
        copy_vanilla: bool,
    ) -> Result<InstalledVersion, MdtError> {
        eprintln!("[mdt] full_install: ensuring vanilla {version}");
        let vanilla_dir = self.root.join("versions").join(version);
        let vanilla_dir = if vanilla_dir.join(format!("{version}.json")).exists() {
            eprintln!("[mdt] full_install: vanilla {version} already present, skipping");
            vanilla_dir
        } else {
            eprintln!("[mdt] full_install: installing vanilla {version}");
            let installed = self.install_vanilla(version).await?;
            eprintln!("[mdt] full_install: vanilla {version} installed");
            // Point at the id the manifest actually reported (matches `version` in the common case).
            self.root.join("versions").join(&installed.id)
        };
        eprintln!(
            "[mdt] full_install: installing loader {} {} (copy_vanilla={copy_vanilla})",
            loader.display_name(),
            loader_version
        );
        let result = self
            .install_modloader(&vanilla_dir, loader, loader_version, copy_vanilla)
            .await;
        eprintln!("[mdt] full_install: loader install finished");
        result
    }

    /// Revert a mod loader back to pure vanilla, using a SHA-1 manifest comparison.
    ///
    /// The base Minecraft version is read from `vanilla_dir`, its official manifest is fetched and
    /// pre-loaded into memory, and every file is compared against it. Modified files are restored,
    /// missing files are re-downloaded, and files the loader added are removed (the `mods/`
    /// directory is preserved). Finally the vanilla JSON is rewritten and the whole directory is
    /// re-verified.
    ///
    /// Safe to re-run: against an already-vanilla directory it is a no-op.
    ///
    /// # Errors
    ///
    /// * [`MdtError::InvalidManifest`] if `vanilla_dir` has no readable version JSON.
    /// * [`MdtError::VersionNotFound`] if the base version isn't in the manifest.
    /// * [`MdtError::UninstallFailed`] if a file fails post-restore verification.
    pub async fn uninstall_modloader(
        &self,
        vanilla_dir: &Path,
        loader: ModLoader,
    ) -> Result<(), MdtError> {
        installer::uninstall_modloader(
            self.downloader.as_ref(),
            &self.root,
            vanilla_dir,
            loader,
            &self.mirror,
            self.concurrency,
            self.reporter.as_ref(),
        )
        .await
    }

    /// Set up or switch a Fabric + Quilt dual-loader instance.
    ///
    /// Operates on `version_dir` (`versions/<mc_version>`) in place, keeping the pristine vanilla
    /// JSON as `<mc_version>.json.mcvanila`. Three scenarios:
    ///
    /// * **Vanilla** — installs Fabric then Quilt, and leaves Quilt active.
    /// * **Single loader** — installs the missing loader (latest stable when no version is given)
    ///   and makes it active.
    /// * **Dual loader** — no install; copies the inactive loader's JSON over `<mc_version>.json`
    ///   to switch context.
    ///
    /// When `fabric_version` / `quilt_version` are `None`, the latest stable version is fetched
    /// from the loader's manifest.
    ///
    /// # Errors
    ///
    /// * [`MdtError::InvalidManifest`] if `version_dir` has no valid name.
    /// * [`MdtError::LoaderInstallFailed`] if a version can't be resolved or installed.
    pub async fn setup_dual_loader(
        &self,
        version_dir: &Path,
        fabric_version: Option<&str>,
        quilt_version: Option<&str>,
    ) -> Result<DualLoaderInfo, MdtError> {
        let mc_version = version_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                MdtError::InvalidManifest(format!(
                    "version directory has no valid name: {}",
                    version_dir.display()
                ))
            })?
            .to_string();

        let state = dual_loader::detect_dual_loader_state(version_dir);
        let active = match state {
            DualLoaderState::Vanilla => {
                let fabric = self
                    .resolve_loader_version(ModLoader::Fabric, fabric_version)
                    .await?;
                let quilt = self
                    .resolve_loader_version(ModLoader::Quilt, quilt_version)
                    .await?;
                self.install_dual_loader(version_dir, ModLoader::Fabric, &fabric).await?;
                self.install_dual_loader(version_dir, ModLoader::Quilt, &quilt).await?;
                ModLoader::Quilt
            }
            DualLoaderState::Single { present } => {
                let other = dual_loader::opposite(present);
                // Preserve the present loader's canonical JSON if a legacy in-place install
                // hasn't produced one yet.
                self.ensure_canonical_json(version_dir, &mc_version, present).await?;
                let other_provided = match other {
                    ModLoader::Fabric => fabric_version,
                    ModLoader::Quilt => quilt_version,
                    _ => None,
                };
                let other_version = self.resolve_loader_version(other, other_provided).await?;
                self.install_dual_loader(version_dir, other, &other_version).await?;
                other
            }
            DualLoaderState::Dual => {
                // Switch context to the inactive loader.
                let current = dual_loader::active_loader(version_dir, &mc_version)
                    .unwrap_or(ModLoader::Fabric);
                let target = dual_loader::opposite(current);
                dual_loader::switch_loader_context(version_dir, target).await?;
                target
            }
        };

        let fabric_version = dual_loader::installed_loader_version(version_dir, &mc_version, ModLoader::Fabric);
        let quilt_version = dual_loader::installed_loader_version(version_dir, &mc_version, ModLoader::Quilt);

        Ok(DualLoaderInfo {
            mc_version,
            fabric_version,
            quilt_version,
            active,
        })
    }

    /// Resolve a loader version: use `provided`, or fetch the latest stable from the manifest.
    async fn resolve_loader_version(
        &self,
        loader: ModLoader,
        provided: Option<&str>,
    ) -> Result<String, MdtError> {
        if let Some(v) = provided {
            return Ok(v.to_string());
        }
        let entries = match loader {
            ModLoader::Fabric => {
                manifest::fetch_fabric_manifest(self.downloader.as_ref(), &self.mirror).await?.loader
            }
            ModLoader::Quilt => {
                manifest::fetch_quilt_manifest(self.downloader.as_ref(), &self.mirror).await?.loader
            }
            other => {
                return Err(MdtError::LoaderInstallFailed(format!(
                    "dual loader does not support {}",
                    other.display_name()
                )))
            }
        };
        latest_stable_loader_version(&entries).ok_or_else(|| {
            MdtError::LoaderInstallFailed(format!("no stable {} version found", loader.display_name()))
        })
    }

    /// Install `loader` in place, then keep its `<mc_version>+<Loader><ver>.json` config around
    /// (the in-place install leaves it only as `<mc_version>.json`).
    async fn install_dual_loader(
        &self,
        version_dir: &Path,
        loader: ModLoader,
        version: &str,
    ) -> Result<(), MdtError> {
        let installed = self.install_modloader(version_dir, loader, version, false).await?;
        let canonical = version_dir.join(format!("{}.json", installed.id));
        tokio::fs::copy(&installed.version_json, &canonical).await?;
        Ok(())
    }

    /// Ensure the present loader has a canonical `<mc_version>+<Loader><ver>.json` file, copying
    /// the active JSON when it doesn't (a legacy in-place install keeps the config only under
    /// `<mc_version>.json`). The loader version is read from the JSON's `libraries`, not its `id`.
    async fn ensure_canonical_json(
        &self,
        version_dir: &Path,
        mc_version: &str,
        loader: ModLoader,
    ) -> Result<(), MdtError> {
        if dual_loader::find_loader_json(version_dir, mc_version, loader).is_some() {
            return Ok(());
        }
        let active = version_dir.join(format!("{mc_version}.json"));
        let Some(version) = dual_loader::loader_version_from_json(&active, loader) else {
            return Ok(());
        };
        let canonical = version_dir.join(format!(
            "{mc_version}+{}{}.json",
            loader.display_name(),
            version
        ));
        tokio::fs::copy(&active, &canonical).await?;
        Ok(())
    }

    /// Detect the dual-loader state of `version_dir` (Vanilla / Single / Dual).
    pub fn detect_dual_loader_state(&self, version_dir: &Path) -> DualLoaderState {
        dual_loader::detect_dual_loader_state(version_dir)
    }

    /// Switch the active loader context of `version_dir` to `target_loader` (Fabric or Quilt).
    ///
    /// Copies the target loader's canonical JSON over `<mc_version>.json`. The `.mcvanila` backup
    /// and both loader JSONs are left untouched. The copy retries on a transient Windows sharing
    /// violation (error 32), since the active JSON is commonly held by Defender or a launcher.
    ///
    /// # Panics
    ///
    /// This method never panics — every failure is returned as `Err`. Do **not** `.unwrap()` the
    /// result unless you have already verified the target loader is installed (for example via
    /// [`Self::detect_dual_loader_state`]).
    ///
    /// # Errors
    ///
    /// * [`MdtError::InvalidManifest`] if `version_dir` has no valid name, or if `target_loader`
    ///   is not installed. **Before calling this method, make sure the target loader is
    ///   installed** — install it with [`Self::setup_dual_loader`] and check the state with
    ///   [`Self::detect_dual_loader_state`].
    pub async fn switch_dual_loader_context(
        &self,
        version_dir: &Path,
        target_loader: ModLoader,
    ) -> Result<(), MdtError> {
        dual_loader::switch_loader_context(version_dir, target_loader).await
    }
}

/// Builder for [`MdtClient`].
pub struct MdtClientBuilder {
    mirror: Mirror,
    root: PathBuf,
    concurrency: usize,
    reporter: Option<Arc<dyn ProgressReporter>>,
    downloader: Option<Arc<dyn Downloader>>,
    proxy: Option<String>,
    connect_timeout: Duration,
    read_timeout: Duration,
}

impl MdtClientBuilder {
    /// Start building with defaults ([`Mirror::BMCLAPI`], root `.`, 3 concurrent downloads).
    #[must_use]
    pub fn new() -> Self {
        Self {
            mirror: Mirror::default(),
            root: PathBuf::from("."),
            concurrency: DEFAULT_CONCURRENCY,
            reporter: None,
            downloader: None,
            proxy: None,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
        }
    }

    /// Pick a mirror (default: [`Mirror::BMCLAPI`]).
    #[must_use]
    pub fn mirror(mut self, mirror: Mirror) -> Self {
        self.mirror = mirror;
        self
    }

    /// Set the target `.minecraft` root directory (default: `.`).
    #[must_use]
    pub fn root_dir(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = root.into();
        self
    }

    /// Override the download concurrency (clamped to at least 1; default 3).
    #[must_use]
    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// Override the progress/status reporter (default: [`StderrReporter`]).
    ///
    /// Implement [`ProgressReporter`] to send MDT's `Downloading`/`Downloaded`/`Updating` events to
    /// your own logger, TUI, or a silent sink.
    #[must_use]
    pub fn progress_reporter(mut self, reporter: impl ProgressReporter + 'static) -> Self {
        self.reporter = Some(Arc::new(reporter));
        self
    }

    /// Override the transport (default: [`HttpDownloader`] over the shared HTTP client).
    ///
    /// Implement [`Downloader`] to serve downloads from a custom source — a caching proxy, an
    /// in-memory store, an alternate protocol — for full low-level control.
    #[must_use]
    pub fn downloader(mut self, downloader: impl Downloader + 'static) -> Self {
        self.downloader = Some(Arc::new(downloader));
        self
    }

    /// Route all requests through an HTTP proxy, e.g. `http://127.0.0.1:7890`.
    #[must_use]
    pub fn proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = Some(url.into());
        self
    }

    /// Set the TCP connect timeout (default 15s).
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set the per-read timeout (default 30s). It resets after every successful read.
    #[must_use]
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Finish building: creates the root directory and the shared HTTP client.
    pub fn build(self) -> Result<MdtClient, MdtError> {
        std::fs::create_dir_all(&self.root)?;
        let http = http::build_client(self.proxy.as_deref(), self.connect_timeout, self.read_timeout)?;
        let downloader = self
            .downloader
            .unwrap_or_else(|| Arc::new(HttpDownloader::new(http.clone())));
        Ok(MdtClient {
            http,
            downloader,
            mirror: self.mirror,
            root: self.root,
            concurrency: self.concurrency,
            reporter: self.reporter.unwrap_or_else(|| Arc::new(StderrReporter::new())),
            proxy: self.proxy,
            connect_timeout: self.connect_timeout,
            read_timeout: self.read_timeout,
        })
    }
}

impl Default for MdtClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_defaults_to_three_threads() {
        let client = MdtClient::builder().build().unwrap();
        assert_eq!(client.concurrency(), 3);
    }

    #[test]
    fn builder_concurrency_is_clamped_to_at_least_one() {
        let client = MdtClient::builder().concurrency(0).build().unwrap();
        assert_eq!(client.concurrency(), 1);
    }

    #[test]
    fn builder_concurrency_can_be_overridden() {
        let client = MdtClient::builder().concurrency(8).build().unwrap();
        assert_eq!(client.concurrency(), 8);
    }

    #[test]
    fn set_concurrency_updates_at_runtime() {
        let mut client = MdtClient::builder().build().unwrap();
        client.set_concurrency(6);
        assert_eq!(client.concurrency(), 6);
    }

    #[test]
    fn set_concurrency_clamps_to_at_least_one() {
        let mut client = MdtClient::builder().build().unwrap();
        client.set_concurrency(0);
        assert_eq!(client.concurrency(), 1);
    }
}
