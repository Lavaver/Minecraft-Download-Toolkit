//! Version-manifest types and fetch logic for Minecraft and its mod loaders.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::MdtError;
use crate::mc_version::McVersion;
use crate::mirror::Mirror;
use crate::modloader::ModLoader;
use crate::urls::ManifestUrls;

// ===== Minecraft version manifest =====

/// The latest release and snapshot versions.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LatestMinecraftVersion {
    pub release: String,
    pub snapshot: String,
}

/// The root of Mojang's `version_manifest.json`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MinecraftVersionsList {
    pub latest: LatestMinecraftVersion,
    pub versions: Vec<VersionEntry>,
}

/// A single Minecraft version entry in the manifest.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VersionEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub version_type: VersionType,
    pub url: String,
    pub time: String,
    pub release_time: String,
}

/// The release channel of a Minecraft version.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VersionType {
    Release,
    Snapshot,
    OldBeta,
    OldAlpha,
    #[serde(other)]
    Other,
}

// ===== Forge / Fabric / Quilt manifests =====

/// Forge version manifest: `mc_version -> [full version strings]`.
pub type ForgeVersionsList = HashMap<String, Vec<String>>;

/// A single loader version entry (Fabric / Quilt). Only `version` is consumed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LoaderVersionEntry {
    pub version: String,
}

/// Fabric meta v2 manifest (only `loader` is consumed).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct FabricVersionsList {
    #[serde(default)]
    pub loader: Vec<LoaderVersionEntry>,
}

/// Quilt meta v3 manifest (only `loader` is consumed).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct QuiltVersionsList {
    #[serde(default)]
    pub loader: Vec<LoaderVersionEntry>,
}

// ===== Maven XML manifests (NeoForge / LiteLoader) =====

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MavenMetadata {
    pub versioning: MavenVersioning,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MavenVersioning {
    pub latest: Option<String>,
    pub release: Option<String>,
    pub versions: MavenVersions,
    #[serde(rename = "lastUpdated")]
    pub last_updated: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MavenVersions {
    #[serde(rename = "version", default)]
    pub version: Vec<String>,
}

// ===== LiteLoader (cross-mirror normalized) =====

/// A LiteLoader version entry, normalized across mirror formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteLoaderVersionEntry {
    pub mcversion: String,
    pub loader_version: String,
    pub file: Option<String>,
    pub md5: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct BmclapiLiteLoaderBuild {
    file: String,
    version: String,
    md5: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct BmclapiLiteLoaderItem {
    mcversion: String,
    build: BmclapiLiteLoaderBuild,
    #[allow(dead_code)]
    version: String,
}

// ===== fetching =====

async fn get_text(http: &reqwest::Client, url: &str) -> Result<String, MdtError> {
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.text().await?)
}

/// Try each candidate URL in order; return the first success, otherwise the last error.
async fn fetch_text(
    http: &reqwest::Client,
    candidates: &[&str],
    label: &str,
) -> Result<String, MdtError> {
    let mut last: Option<MdtError> = None;
    for &url in candidates {
        match get_text(http, url).await {
            Ok(text) => return Ok(text),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| MdtError::InvalidManifest(format!("no reachable mirror for {label}"))))
}

/// Fetch the Minecraft version manifest.
pub async fn fetch_minecraft_version_manifest(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<MinecraftVersionsList, MdtError> {
    eprintln!("Updating {} index", mirror.label());
    let primary = ManifestUrls::try_new(mirror)?;
    let fallback = ManifestUrls::try_new(&mirror.other())?;
    let candidates = [primary.minecraft.as_str(), fallback.minecraft.as_str()];
    let text = fetch_text(http, &candidates, "Minecraft version manifest").await?;
    Ok(serde_json::from_str(&text)?)
}

/// Fetch and parse the version JSON for a single Minecraft version.
///
/// Returns the raw text (for round-tripping back to disk) and the parsed
/// [`crate::downloader::VersionJson`].
pub async fn fetch_version_json(
    http: &reqwest::Client,
    mirror: &Mirror,
    version: &str,
) -> Result<(String, crate::downloader::VersionJson), MdtError> {
    let manifest = fetch_minecraft_version_manifest(http, mirror).await?;
    let entry = manifest
        .versions
        .iter()
        .find(|v| v.id == version)
        .cloned()
        .ok_or_else(|| MdtError::VersionNotFound(version.to_string()))?;

    let urls = ManifestUrls::try_new(mirror)?;
    let version_url = urls.apply_mirror(&entry.url, mirror);
    let text = http
        .get(&version_url)
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let version_json: crate::downloader::VersionJson = serde_json::from_str(&text)
        .map_err(|e| MdtError::JsonParseError(e).context(format!("parsing version JSON for {version}")))?;
    Ok((text, version_json))
}

/// Fetch the Forge version manifest.
pub async fn fetch_forge_manifest(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<ForgeVersionsList, MdtError> {
    let primary = ManifestUrls::try_new(mirror)?;
    let fallback = ManifestUrls::try_new(&mirror.other())?;
    let candidates = [primary.forge.as_str(), fallback.forge.as_str()];
    let text = fetch_text(http, &candidates, "Forge manifest").await?;
    Ok(serde_json::from_str(&text)?)
}

/// Fetch the Fabric version manifest.
pub async fn fetch_fabric_manifest(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<FabricVersionsList, MdtError> {
    let primary = ManifestUrls::try_new(mirror)?;
    let fallback = ManifestUrls::try_new(&mirror.other())?;
    let candidates = [primary.fabric.as_str(), fallback.fabric.as_str()];
    let text = fetch_text(http, &candidates, "Fabric manifest").await?;
    Ok(serde_json::from_str(&text)?)
}

/// Fetch the Quilt version manifest.
pub async fn fetch_quilt_manifest(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<QuiltVersionsList, MdtError> {
    let primary = ManifestUrls::try_new(mirror)?;
    let fallback = ManifestUrls::try_new(&mirror.other())?;
    let candidates = [primary.quilt.as_str(), fallback.quilt.as_str()];
    let text = fetch_text(http, &candidates, "Quilt manifest").await?;
    Ok(serde_json::from_str(&text)?)
}

/// Fetch the NeoForge version manifest (Maven XML).
pub async fn fetch_neoforge_manifest(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<MavenMetadata, MdtError> {
    let primary = ManifestUrls::try_new(mirror)?;
    let fallback = ManifestUrls::try_new(&mirror.other())?;
    let candidates = [primary.neoforge.as_str(), fallback.neoforge.as_str()];
    let text = fetch_text(http, &candidates, "NeoForge manifest").await?;
    Ok(quick_xml::de::from_str(&text)?)
}

/// Fetch the LiteLoader version manifest, falling back to the opposite mirror on failure.
pub async fn fetch_liteloader_manifest(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<Vec<LiteLoaderVersionEntry>, MdtError> {
    match fetch_liteloader_as(http, mirror).await {
        Ok(entries) => Ok(entries),
        Err(_) => fetch_liteloader_as(http, &mirror.other()).await,
    }
}

async fn fetch_liteloader_as(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<Vec<LiteLoaderVersionEntry>, MdtError> {
    let urls = ManifestUrls::try_new(mirror)?;
    match mirror {
        // Custom mirrors are assumed to serve the official Maven-XML format, like Mojang.
        Mirror::Mojang | Mirror::Custom(_) => {
            let text = fetch_text(http, &[urls.liteloader.as_str()], "LiteLoader manifest").await?;
            let meta: MavenMetadata = quick_xml::de::from_str(&text)?;
            Ok(meta
                .versioning
                .versions
                .version
                .into_iter()
                .map(|ver| {
                    let mcversion = ver.split('-').next().unwrap_or(&ver).to_string();
                    LiteLoaderVersionEntry {
                        mcversion,
                        loader_version: ver,
                        file: None,
                        md5: None,
                    }
                })
                .collect())
        }
        Mirror::BMCLAPI => {
            let text = fetch_text(http, &[urls.liteloader.as_str()], "LiteLoader manifest").await?;
            let entries: Vec<BmclapiLiteLoaderItem> = serde_json::from_str(&text)?;
            Ok(entries
                .into_iter()
                .map(|e| LiteLoaderVersionEntry {
                    mcversion: e.mcversion,
                    loader_version: e.build.version,
                    file: Some(e.build.file),
                    md5: Some(e.build.md5),
                })
                .collect())
        }
    }
}

// ===== aggregated manifests =====

/// All manifests, fetched concurrently.
pub struct ManifestData {
    pub minecraft: MinecraftVersionsList,
    pub forge: ForgeVersionsList,
    pub fabric: FabricVersionsList,
    pub quilt: QuiltVersionsList,
    pub neoforge: MavenMetadata,
    pub liteloader: Vec<LiteLoaderVersionEntry>,
}

/// Fetch every manifest concurrently.
pub async fn fetch_all_manifests(
    http: &reqwest::Client,
    mirror: &Mirror,
) -> Result<ManifestData, MdtError> {
    let (minecraft, forge, fabric, quilt, neoforge, liteloader) = tokio::try_join!(
        fetch_minecraft_version_manifest(http, mirror),
        fetch_forge_manifest(http, mirror),
        fetch_fabric_manifest(http, mirror),
        fetch_quilt_manifest(http, mirror),
        fetch_neoforge_manifest(http, mirror),
        fetch_liteloader_manifest(http, mirror),
    )?;

    Ok(ManifestData {
        minecraft,
        forge,
        fabric,
        quilt,
        neoforge,
        liteloader,
    })
}

/// Derive, in memory, the loader versions compatible with a Minecraft version.
///
/// * **Forge** — read directly from the version map, stripping the `{mc}-` prefix.
/// * **Fabric / Quilt** — return the full loader version list (decoupled from MC versions).
/// * **NeoForge** — filter Maven versions by the [`neoforge_mc_prefix`] heuristic.
/// * **LiteLoader** — filter by the manifest's `mcversion`.
pub fn loader_versions_from_data(
    loader: ModLoader,
    mc_version: &str,
    data: &ManifestData,
) -> Vec<String> {
    match loader {
        ModLoader::Forge => {
            let prefix = format!("{mc_version}-");
            let Some(versions) = data.forge.get(mc_version) else {
                return Vec::new();
            };
            versions
                .iter()
                .map(|v| {
                    v.strip_prefix(&prefix)
                        .map(str::to_string)
                        .unwrap_or_else(|| v.clone())
                })
                .collect()
        }
        ModLoader::Fabric => data.fabric.loader.iter().map(|l| l.version.clone()).collect(),
        ModLoader::Quilt => data.quilt.loader.iter().map(|l| l.version.clone()).collect(),
        ModLoader::Neoforge => {
            let prefix = neoforge_mc_prefix(mc_version);
            data.neoforge
                .versioning
                .versions
                .version
                .iter()
                .filter(|v| match &prefix {
                    Some(p) => v.starts_with(p.as_str()),
                    None => true,
                })
                .cloned()
                .collect()
        }
        ModLoader::Liteloader => data
            .liteloader
            .iter()
            .filter(|e| e.mcversion == mc_version)
            .map(|e| e.loader_version.clone())
            .collect(),
    }
}

/// Derive the NeoForge Maven version prefix from a Minecraft version (heuristic).
///
/// NeoForge's numbering:
/// * `1.20.1` → `47.x`, `1.20.2` → `48.x`, `1.20.3` → `49.x` (carried over from Forge).
/// * Old-style `1.minor[.patch]` → `minor[.patch].` (e.g. `1.20.4` → `20.4.`).
/// * New-style `[year].minor[.patch]` (26.x onward) → `year.minor[.patch].` verbatim.
///
/// This mapping is not guaranteed to be exact; pass an explicit loader version when in doubt.
pub fn neoforge_mc_prefix(mc_version: &str) -> Option<String> {
    match mc_version {
        "1.20.1" => return Some("47.".to_string()),
        "1.20.2" => return Some("48.".to_string()),
        "1.20.3" => return Some("49.".to_string()),
        _ => {}
    }

    let v = McVersion::parse(mc_version)?;
    if v.major == 1 {
        // Old-style: strip the leading `1.`
        if v.patch > 0 {
            Some(format!("{}.{}.", v.minor, v.patch))
        } else {
            Some(format!("{}.", v.minor))
        }
    } else {
        // New-style (26.x onward): keep the full `year.minor[.patch]`
        if v.patch > 0 {
            Some(format!("{}.{}.{}.", v.major, v.minor, v.patch))
        } else {
            Some(format!("{}.{}.", v.major, v.minor))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn fetch_text_falls_back_between_candidates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/primary"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fallback"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let http = test_client();
        let primary = format!("{}/primary", server.uri());
        let fallback = format!("{}/fallback", server.uri());
        let text = fetch_text(&http, &[primary.as_str(), fallback.as_str()], "test")
            .await
            .unwrap();
        assert_eq!(text, "ok");
    }

    #[tokio::test]
    async fn fetch_text_returns_last_error_when_all_candidates_fail() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/b"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let http = test_client();
        let a = format!("{}/a", server.uri());
        let b = format!("{}/b", server.uri());
        let err = fetch_text(&http, &[a.as_str(), b.as_str()], "test").await.unwrap_err();
        assert!(matches!(err, MdtError::NetworkError(_)), "got {err:?}");
    }
}
