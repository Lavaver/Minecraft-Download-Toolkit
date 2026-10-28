//! Method B: manual unpack / version-JSON synthesis.
//!
//! No Java runtime is required: deploy the extracted core loader JAR and its dependencies into
//! `libraries/`, then hand-write the Minecraft version JSON.

use std::path::Path;

use serde::Serialize;

use crate::customize::{Downloader, ProgressReporter};
use crate::error::MdtError;
use crate::mirror::Mirror;
use crate::modloader::ModLoader;
use crate::registry;
use crate::urls::ManifestUrls;

use super::{
    download_artifact_verified, download_maven_fallback, ensure_empty_dir, libraries_dir,
    library_path_from_name, loader_version_id, target_version_dir, unzip_jar, version_dir,
    InstallMethod, InstallRequest, InstalledVersion,
};

// ===== generated version JSON structures =====

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct GeneratedArtifact {
    pub path: String,
    pub url: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct GeneratedLibraryDownloads {
    pub artifact: GeneratedArtifact,
}

/// A single dependency library in the modern `downloads.artifact` format.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct GeneratedLibrary {
    pub name: String,
    pub downloads: GeneratedLibraryDownloads,
}

impl GeneratedLibrary {
    /// Build from a Maven coordinate plus a repository base URL.
    pub fn maven(name: &str, base_url: &str) -> Result<Self, MdtError> {
        let path = library_path_from_name(name)?;
        let base = base_url.trim_end_matches('/');
        Ok(Self {
            name: name.to_string(),
            downloads: GeneratedLibraryDownloads {
                artifact: GeneratedArtifact {
                    url: format!("{base}/{path}"),
                    path,
                },
            },
        })
    }

    /// Build with an explicit `path` + `url` (for non-standard repositories like LiteLoader).
    pub fn direct(name: &str, path: &str, url: &str) -> Self {
        Self {
            name: name.to_string(),
            downloads: GeneratedLibraryDownloads {
                artifact: GeneratedArtifact {
                    path: path.to_string(),
                    url: url.to_string(),
                },
            },
        }
    }
}

/// A hand-written Minecraft version JSON.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct GeneratedVersionJson {
    pub id: String,
    #[serde(rename = "inheritsFrom", skip_serializing_if = "Option::is_none")]
    pub inherits_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(rename = "releaseTime", skip_serializing_if = "Option::is_none")]
    pub release_time: Option<String>,
    #[serde(rename = "type")]
    pub version_type: String,
    #[serde(rename = "mainClass")]
    pub main_class: String,
    #[serde(rename = "minecraftArguments", skip_serializing_if = "Option::is_none")]
    pub minecraft_arguments: Option<String>,
    pub libraries: Vec<GeneratedLibrary>,
}

impl GeneratedVersionJson {
    pub fn to_pretty_json(&self) -> Result<String, MdtError> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

// ===== mirror rewriting =====

/// Recursively rewrite every `"url"` string in a JSON value through the active mirror.
pub fn apply_mirror_to_json(
    mut value: serde_json::Value,
    mirror: &Mirror,
    urls: &ManifestUrls,
) -> serde_json::Value {
    match &mut value {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if key == "url" {
                    if let serde_json::Value::String(s) = val {
                        let mirrored = urls.apply_mirror(&urls.apply_maven_mirror(s, mirror), mirror);
                        *s = mirrored;
                    }
                } else {
                    let taken = std::mem::take(val);
                    *val = apply_mirror_to_json(taken, mirror, urls);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                let taken = std::mem::take(item);
                *item = apply_mirror_to_json(taken, mirror, urls);
            }
        }
        _ => {}
    }
    value
}

// ===== shared deploy logic =====

/// Download a single library by Maven coordinate into `libraries/`, skipping if present.
///
/// `by` is an optional attribution for progress output (e.g. `Fabric 0.20.0`); when set, a
/// `Downloaded <file> by <by>` line is printed after the file lands.
///
/// When `registry_dir` is set, the file is cached globally under it (keyed by Maven path) and
/// *copied* into `libraries/`, so re-installing the same loader never re-downloads. The cache is
/// checksum-verified (authoritative Maven `.sha1`/`.sha256`/`.md5`, then persisted for offline use).
pub async fn download_library(
    downloader: &dyn Downloader,
    name: &str,
    base_url: &str,
    root: &Path,
    mirror: &Mirror,
    urls: &ManifestUrls,
    reporter: &dyn ProgressReporter,
    by: Option<&str>,
    registry_dir: Option<&Path>,
) -> Result<(), MdtError> {
    let rel = library_path_from_name(name)?;
    let dest = libraries_dir(root).join(&rel);
    if dest.exists() {
        eprintln!("[mdt] download_library: {name} already present, skipping");
        return Ok(());
    }
    let upstream = format!("{}/{}", base_url.trim_end_matches('/'), rel);
    eprintln!("[mdt] download_library: {name} from {upstream}");

    download_artifact_verified(downloader, &upstream, &dest, mirror, urls, registry_dir, &rel).await?;

    if let Some(by) = by {
        let file_name = rel.rsplit('/').next().unwrap_or(rel.as_str());
        reporter.status(&format!("Downloaded {file_name} by {by}"));
    }
    Ok(())
}

/// Fetch a loader profile JSON, trying each candidate URL in order and returning the first success
/// together with the URL that served it.
async fn fetch_profile_json(
    downloader: &dyn Downloader,
    candidates: &[String],
) -> Result<(String, String), MdtError> {
    let mut last_err = None;
    for url in candidates {
        eprintln!("[mdt] install_from_profile: fetching {url}");
        match downloader.fetch(url).await {
            Ok(text) => {
                eprintln!("[mdt] install_from_profile: profile body read ({} bytes)", text.len());
                return Ok((text, url.clone()));
            }
            Err(e) => {
                eprintln!("[mdt] install_from_profile: {url} failed, trying next: {e}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        MdtError::InvalidManifest("no reachable loader profile source".to_string())
    }))
}

/// Swap a freshly-written Method B loader JSON into the vanilla JSON's filename for an in-place
/// install, so launchers recognize it (the JSON name must match the vanilla JAR name).
///
/// The original vanilla JSON is backed up as `<mc_version>.json.mcvanila` for the uninstall flow.
/// The backup is created only once: a pre-existing `.mcvanila` (from an earlier in-place install)
/// is left intact, so re-installing a loader never clobbers the real vanilla JSON.
pub(super) async fn swap_loader_json_in_place(
    target_dir: &Path,
    mc_version: &str,
    loader_id: &str,
) -> Result<(), MdtError> {
    let vanilla_json = target_dir.join(format!("{mc_version}.json"));
    let loader_json = target_dir.join(format!("{loader_id}.json"));
    let backup_json = target_dir.join(format!("{mc_version}.json.mcvanila"));

    if vanilla_json.exists() && !backup_json.exists() {
        tokio::fs::rename(&vanilla_json, &backup_json).await?;
    }
    tokio::fs::rename(&loader_json, &vanilla_json).await?;
    Ok(())
}

/// Method B entry point for profile-based loaders (Fabric / Quilt).
///
/// `profile_urls` is ordered mirror-first, official-last; the first URL that returns a parseable
/// profile wins, so a stalled mirror falls through to the official source.
pub async fn install_from_profile(
    downloader: &dyn Downloader,
    profile_urls: &[String],
    req: &InstallRequest,
    urls: &ManifestUrls,
) -> Result<InstalledVersion, MdtError> {
    let (text, used_url) = fetch_profile_json(downloader, profile_urls).await?;
    let mut profile: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        MdtError::JsonParseError(e).context(format!("parsing loader profile {used_url}"))
    })?;

    // Enforce the isolation convention: override the profile's own `id` so the loader lands in
    // `versions/<mc_version>+<Loader><loader_version>` and never overwrites vanilla.
    let id = loader_version_id(&req.mc_version, req.loader, &req.loader_version);
    profile["id"] = serde_json::Value::String(id.clone());
    profile["inheritsFrom"] = serde_json::Value::String(req.mc_version.clone());

    let libs = profile
        .get("libraries")
        .and_then(|v| v.as_array())
        .cloned()
        .ok_or_else(|| MdtError::InvalidManifest("loader profile is missing a `libraries` array".to_string()))?;

    let by = format!("{} {}", req.loader.display_name(), req.loader_version);
    let reg_dir = registry::modloader_dir(req.loader.display_name(), &req.loader_version);
    for lib in &libs {
        let name = lib
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MdtError::InvalidManifest("loader profile library is missing `name`".to_string()))?;
        let base = lib
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MdtError::InvalidManifest("loader profile library is missing `url`".to_string()))?;
        download_library(downloader, name, base, &req.root_dir, &req.mirror, urls, req.reporter.as_ref(), Some(&by), Some(&reg_dir)).await?;
    }

    let profile = apply_mirror_to_json(profile, &req.mirror, urls);
    let vdir = target_version_dir(req, &id);
    tokio::fs::create_dir_all(&vdir).await?;
    let json_path = vdir.join(format!("{id}.json"));
    tokio::fs::write(&json_path, serde_json::to_string_pretty(&profile)?).await?;

    // In-place installs take over the vanilla JSON's filename so the launcher sees the loader
    // version; the vanilla JSON is backed up as `.mcvanila` for the uninstall flow.
    let version_json = if let Some(target) = &req.target_dir {
        swap_loader_json_in_place(target, &req.mc_version, &id).await?;
        target.join(format!("{}.json", req.mc_version))
    } else {
        json_path
    };

    Ok(InstalledVersion {
        id,
        method: InstallMethod::ManualUnpack,
        version_json,
        jar: None,
    })
}

/// Method B for legacy Forge (1.12.2 and earlier): unpack the installer and deploy `maven/`.
pub async fn install_legacy_forge(
    downloader: &dyn Downloader,
    installer_url: &str,
    root: &Path,
    mirror: &Mirror,
    urls: &ManifestUrls,
    mc_version: &str,
    loader_version: &str,
    target_dir: Option<&Path>,
) -> Result<InstalledVersion, MdtError> {
    let temp = root.join(".mdt-forge-unpack");
    ensure_empty_dir(&temp).await?;

    let installer_jar = temp.join("installer.jar");
    download_maven_fallback(downloader, installer_url, &installer_jar, mirror, urls).await?;

    let unpack = temp.join("unpack");
    unzip_jar(&installer_jar, &unpack).await?;

    // 1. Flatten the installer's `maven/` into `libraries/`.
    let maven_src = unpack.join("maven");
    if !maven_src.exists() {
        let _ = tokio::fs::remove_dir_all(&temp).await;
        return Err(MdtError::LoaderInstallFailed(
            "legacy Forge installer is missing `maven/`".to_string(),
        ));
    }
    super::copy_dir_recursive(&maven_src, &libraries_dir(root)).await?;

    // 2. Read and mirror `version.json`.
    let vjson_path = unpack.join("version.json");
    let raw_text = tokio::fs::read_to_string(&vjson_path).await.map_err(|e| {
        MdtError::IoError(e).context(format!("installer is missing version.json at {}", vjson_path.display()))
    })?;
    let mut raw: serde_json::Value = serde_json::from_str(&raw_text)
        .map_err(|e| MdtError::JsonParseError(e).context("parsing legacy Forge version.json"))?;

    // Override the installer's own ID with the isolation convention.
    let id = loader_version_id(mc_version, ModLoader::Forge, loader_version);
    raw["id"] = serde_json::Value::String(id.clone());
    let mirrored = apply_mirror_to_json(raw, mirror, urls);

    // 3. Write the version JSON.
    let vdir = match target_dir {
        Some(t) => t.to_path_buf(),
        None => version_dir(root, &id),
    };
    tokio::fs::create_dir_all(&vdir).await?;
    let json_path = vdir.join(format!("{id}.json"));
    tokio::fs::write(&json_path, serde_json::to_string_pretty(&mirrored)?).await?;

    let version_json = if let Some(t) = target_dir {
        swap_loader_json_in_place(t, mc_version, &id).await?;
        t.join(format!("{mc_version}.json"))
    } else {
        json_path
    };

    let _ = tokio::fs::remove_dir_all(&temp).await;

    Ok(InstalledVersion {
        id,
        method: InstallMethod::ManualUnpack,
        version_json,
        jar: None,
    })
}

/// Read `minecraftArguments` and `mainClass` from an installed vanilla version JSON.
///
/// After an in-place install the pristine vanilla JSON lives at `.json.mcvanila` (the `.json` slot
/// is the loader JSON); read that backup when present so re-installing never appends the tweak
/// class twice.
pub async fn read_vanilla_launch_args(
    root: &Path,
    mc_version: &str,
) -> Result<(Option<String>, Option<String>), MdtError> {
    let dir = version_dir(root, mc_version);
    let vanilla_path = dir.join(format!("{mc_version}.json"));
    let backup_path = dir.join(format!("{mc_version}.json.mcvanila"));
    let path = if backup_path.exists() {
        backup_path
    } else {
        vanilla_path
    };
    let text = tokio::fs::read_to_string(&path).await.map_err(|e| {
        MdtError::IoError(e).context(format!("vanilla version JSON not found at {}", path.display()))
    })?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let args = v.get("minecraftArguments").and_then(|a| a.as_str()).map(String::from);
    let main = v.get("mainClass").and_then(|m| m.as_str()).map(String::from);
    Ok((args, main))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::customize::{HttpDownloader, StderrReporter};
    use sha1::{Digest, Sha1};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mdt-vjson-{tag}-{}-{n}", std::process::id()))
    }

    fn test_client() -> HttpDownloader {
        HttpDownloader::new(reqwest::Client::builder().no_proxy().build().unwrap())
    }

    #[tokio::test]
    async fn download_library_copies_from_modloader_cache_without_network() {
        let root = unique_dir("root");
        let reg_dir = unique_dir("registry");
        let name = "net.fabricmc:fabric-loader:0.15.11";
        let rel = library_path_from_name(name).unwrap();
        let content = b"fake-jar-bytes";
        let hash = hex::encode(Sha1::digest(content));

        // Pre-populate the global modloader cache with the jar AND its persisted checksum sidecar.
        let cached = reg_dir.join(&rel);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, content).unwrap();
        std::fs::write(format!("{}.checksum", cached.display()), format!("{hash}\n")).unwrap();

        let http = HttpDownloader::new(reqwest::Client::new());
        let mirror = Mirror::Mojang;
        let urls = ManifestUrls::try_new(&mirror).unwrap();

        // Verified cache hit: the unreachable base_url must never be contacted.
        download_library(
            &http,
            name,
            "https://example.invalid/",
            &root,
            &mirror,
            &urls,
            &StderrReporter::default(),
            None,
            Some(&reg_dir),
        )
        .await
        .unwrap();

        let dest = libraries_dir(&root).join(&rel);
        assert_eq!(std::fs::read(&dest).unwrap(), content);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&reg_dir).ok();
    }

    #[tokio::test]
    async fn download_library_rejects_corrupted_cache() {
        let root = unique_dir("root");
        let reg_dir = unique_dir("registry");
        let name = "net.fabricmc:fabric-loader:0.15.11";
        let rel = library_path_from_name(name).unwrap();
        let good_hash = hex::encode(Sha1::digest(b"good-bytes"));

        // Cache jar is corrupted, but its sidecar claims the (now wrong) good hash.
        let cached = reg_dir.join(&rel);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"tampered").unwrap();
        std::fs::write(format!("{}.checksum", cached.display()), format!("{good_hash}\n")).unwrap();

        let http = HttpDownloader::new(reqwest::Client::new());
        let mirror = Mirror::Mojang;
        let urls = ManifestUrls::try_new(&mirror).unwrap();

        // The corrupted cache is rejected, so it falls through to a network download, which fails
        // because the base_url is unreachable.
        let err = download_library(
            &http,
            name,
            "https://example.invalid/",
            &root,
            &mirror,
            &urls,
            &StderrReporter::default(),
            None,
            Some(&reg_dir),
        )
        .await
        .unwrap_err();
        assert!(!matches!(err, MdtError::ChecksumMismatch { .. }), "got {err:?}");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&reg_dir).ok();
    }

    #[tokio::test]
    async fn download_library_downloads_and_verifies_via_mock() {
        let server = MockServer::start().await;
        let content = b"fake-loader-jar".to_vec();
        let sha1 = hex::encode(Sha1::digest(&content));
        let jar_path = "/maven/net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar";

        Mock::given(method("GET"))
            .and(path(jar_path))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{jar_path}.sha1").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{sha1}\n")))
            .mount(&server)
            .await;

        let root = unique_dir("dl-verify");
        let reg_dir = unique_dir("dl-verify-registry");
        let http = test_client();
        let mirror = Mirror::Mojang;
        let urls = ManifestUrls::try_new(&mirror).unwrap();

        download_library(
            &http,
            "net.fabricmc:fabric-loader:0.15.11",
            &format!("{}/maven", server.uri()),
            &root,
            &mirror,
            &urls,
            &StderrReporter::default(),
            None,
            Some(&reg_dir),
        )
        .await
        .unwrap();

        let rel = "net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar";
        assert_eq!(std::fs::read(root.join("libraries").join(rel)).unwrap(), content);
        assert_eq!(std::fs::read(reg_dir.join(rel)).unwrap(), content);
        let sidecar = std::fs::read_to_string(format!("{}.checksum", reg_dir.join(rel).display())).unwrap();
        assert_eq!(sidecar.trim(), sha1);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&reg_dir).ok();
    }

    #[tokio::test]
    async fn download_library_rejects_checksum_mismatch() {
        let server = MockServer::start().await;
        let good = b"correct-bytes".to_vec();
        let sha1 = hex::encode(Sha1::digest(&good));
        let jar_path = "/maven/net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar";

        Mock::given(method("GET"))
            .and(path(jar_path))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{jar_path}.sha1").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{sha1}\n")))
            .mount(&server)
            .await;

        let root = unique_dir("dl-mismatch");
        let reg_dir = unique_dir("dl-mismatch-registry");
        let http = test_client();
        let mirror = Mirror::Mojang;
        let urls = ManifestUrls::try_new(&mirror).unwrap();

        let err = download_library(
            &http,
            "net.fabricmc:fabric-loader:0.15.11",
            &format!("{}/maven", server.uri()),
            &root,
            &mirror,
            &urls,
            &StderrReporter::default(),
            None,
            Some(&reg_dir),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MdtError::ChecksumMismatch { .. }), "got {err:?}");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&reg_dir).ok();
    }

    #[tokio::test]
    async fn fetch_profile_json_falls_back_between_candidates() {
        let server = MockServer::start().await;
        let profile = r#"{"id":"x","libraries":[]}"#;

        Mock::given(method("GET"))
            .and(path("/primary.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fallback.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(profile))
            .mount(&server)
            .await;

        let http = test_client();
        let primary = format!("{}/primary.json", server.uri());
        let fallback = format!("{}/fallback.json", server.uri());
        let (text, used) = fetch_profile_json(&http, &[primary, fallback]).await.unwrap();

        assert!(used.ends_with("/fallback.json"), "used {used}");
        assert_eq!(text, profile);
    }

    #[tokio::test]
    async fn swap_loader_json_in_place_moves_loader_and_backs_up_vanilla() {
        let dir = unique_dir("swap");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.16.5.json"), r#"{"id":"1.16.5"}"#).unwrap();
        std::fs::write(
            dir.join("1.16.5+Fabric0.20.0.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","inheritsFrom":"1.16.5"}"#,
        )
        .unwrap();

        swap_loader_json_in_place(&dir, "1.16.5", "1.16.5+Fabric0.20.0")
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("1.16.5.json")).unwrap(),
            r#"{"id":"1.16.5+Fabric0.20.0","inheritsFrom":"1.16.5"}"#
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("1.16.5.json.mcvanila")).unwrap(),
            r#"{"id":"1.16.5"}"#
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn swap_loader_json_in_place_preserves_existing_vanilla_backup() {
        let dir = unique_dir("swap-keep-backup");
        std::fs::create_dir_all(&dir).unwrap();
        // The vanilla JSON was already backed up by an earlier in-place install; the "vanilla"
        // slot currently holds the previous loader JSON.
        std::fs::write(
            dir.join("1.16.5.json"),
            r#"{"id":"1.16.5+Fabric0.20.0","inheritsFrom":"1.16.5"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("1.16.5.json.mcvanila"), r#"{"id":"1.16.5"}"#).unwrap();
        std::fs::write(
            dir.join("1.16.5+Quilt0.24.0.json"),
            r#"{"id":"1.16.5+Quilt0.24.0","inheritsFrom":"1.16.5"}"#,
        )
        .unwrap();

        swap_loader_json_in_place(&dir, "1.16.5", "1.16.5+Quilt0.24.0")
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("1.16.5.json")).unwrap(),
            r#"{"id":"1.16.5+Quilt0.24.0","inheritsFrom":"1.16.5"}"#
        );
        // The original vanilla backup is untouched.
        assert_eq!(
            std::fs::read_to_string(dir.join("1.16.5.json.mcvanila")).unwrap(),
            r#"{"id":"1.16.5"}"#
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_vanilla_launch_args_prefers_mcvanila_backup() {
        let root = unique_dir("launch-args");
        let mc_version = "1.12.2";
        let vdir = root.join("versions").join(mc_version);
        std::fs::create_dir_all(&vdir).unwrap();
        // The `.json` slot is a previous loader JSON (tweak class already appended).
        std::fs::write(
            vdir.join(format!("{mc_version}.json")),
            r#"{"minecraftArguments":"--tweakClass com.mumfrey.liteloader.launch.LiteLoaderTweaker","mainClass":"net.minecraft.launchwrapper.Launch"}"#,
        )
        .unwrap();
        // The pristine vanilla JSON is the `.mcvanila` backup.
        std::fs::write(
            vdir.join(format!("{mc_version}.json.mcvanila")),
            r#"{"minecraftArguments":"--username ${auth_player_name}","mainClass":"net.minecraft.client.main.Main"}"#,
        )
        .unwrap();

        let (args, main) = read_vanilla_launch_args(&root, mc_version).await.unwrap();
        assert_eq!(args.as_deref(), Some("--username ${auth_player_name}"));
        assert_eq!(main.as_deref(), Some("net.minecraft.client.main.Main"));

        std::fs::remove_dir_all(&root).ok();
    }
}
