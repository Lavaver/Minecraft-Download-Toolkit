//! Forge loader installation.
//!
//! * `1.13` and later — Method A (official installer + Java injection).
//! * `1.12.2` and earlier — Method B (manual unpack).

use crate::customize::Downloader;
use crate::error::MdtError;
use crate::mc_version::McVersion;
use crate::modloader::ModLoader;
use crate::registry;
use crate::urls::ManifestUrls;

use super::installer::install_via_official_installer;
use super::version_json::install_legacy_forge;
use super::{
    installer_version_id, relocate_method_a, version_dir, InstallMethod, InstallRequest,
    InstalledVersion,
};

/// The version at which Forge introduced the modern installer (`--installClient`).
const MODERN_FORGE_MIN: McVersion = McVersion::new(1, 13, 0);

/// Normalize a Forge version: manifest versions carry the MC prefix (e.g. `1.20.1-47.2.0`),
/// whereas install logic expects a bare Forge version (e.g. `47.2.0`). Strip the prefix if
/// present, otherwise return the input unchanged.
fn normalize_forge_version(mc_version: &str, loader_version: &str) -> String {
    let prefix = format!("{mc_version}-");
    loader_version
        .strip_prefix(&prefix)
        .map(str::to_string)
        .unwrap_or_else(|| loader_version.to_string())
}

pub async fn install(
    downloader: &dyn Downloader,
    req: InstallRequest,
) -> Result<InstalledVersion, MdtError> {
    let urls = ManifestUrls::try_new(&req.mirror)?;

    // Unparseable versions (e.g. snapshots) are treated as modern.
    let use_method_a = match McVersion::parse(&req.mc_version) {
        Some(v) => v >= MODERN_FORGE_MIN,
        None => true,
    };

    if use_method_a {
        install_modern(downloader, req, &urls).await
    } else {
        install_legacy(downloader, req, &urls).await
    }
}

async fn install_modern(
    downloader: &dyn Downloader,
    req: InstallRequest,
    urls: &ManifestUrls,
) -> Result<InstalledVersion, MdtError> {
    let forge_version = normalize_forge_version(&req.mc_version, &req.loader_version);
    let id = installer_version_id(&req.mc_version, ModLoader::Forge, &forge_version);
    let maven_version = format!("{}-{}", req.mc_version, forge_version);
    let installer_url = format!(
        "{}/net/minecraftforge/forge/{}/forge-{}-installer.jar",
        urls.forge_maven, maven_version, maven_version
    );
    let rel_path = format!(
        "net/minecraftforge/forge/{maven_version}/forge-{maven_version}-installer.jar"
    );
    let reg_dir = registry::modloader_dir(req.loader.display_name(), &forge_version);

    install_via_official_installer(
        downloader,
        &installer_url,
        &req.root_dir,
        &req.mirror,
        urls,
        Some(&reg_dir),
        &rel_path,
    )
    .await?;

    // Verify: the official installer should produce a version JSON. Modern installers reuse the
    // vanilla client JAR via `inheritsFrom`, so only the JSON is required.
    let vdir = version_dir(&req.root_dir, &id);
    let json_path = vdir.join(format!("{id}.json"));
    if !json_path.exists() {
        return Err(MdtError::LoaderInstallFailed(format!(
            "official installer finished but produced no version JSON at {}",
            json_path.display()
        )));
    }

    // In-place installs relocate the installer's output into the vanilla directory.
    let (version_json, jar) = match &req.target_dir {
        Some(target) => relocate_method_a(&req.root_dir, &id, target).await?,
        None => {
            let jar_path = vdir.join(format!("{id}.jar"));
            let jar = if jar_path.exists() {
                Some(jar_path)
            } else {
                None
            };
            (json_path, jar)
        }
    };

    Ok(InstalledVersion {
        id,
        method: InstallMethod::JavaInjection,
        version_json,
        jar,
    })
}

async fn install_legacy(
    downloader: &dyn Downloader,
    req: InstallRequest,
    urls: &ManifestUrls,
) -> Result<InstalledVersion, MdtError> {
    let forge_version = normalize_forge_version(&req.mc_version, &req.loader_version);
    let maven_version = format!("{}-{}", req.mc_version, forge_version);
    let installer_url = format!(
        "{}/net/minecraftforge/forge/{}/forge-{}-installer.jar",
        urls.forge_maven, maven_version, maven_version
    );
    install_legacy_forge(
        downloader,
        &installer_url,
        &req.root_dir,
        &req.mirror,
        urls,
        &req.mc_version,
        &forge_version,
        req.target_dir.as_deref(),
    )
    .await
}
