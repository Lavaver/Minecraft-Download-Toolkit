//! LiteLoader loader installation (Method B: manual unpack).
//!
//! LiteLoader depends on LaunchWrapper and injects itself into the vanilla launch flow via
//! `--tweakClass`. MDT downloads the core JAR and its dependencies, then synthesizes the version
//! JSON — no Java required.

use crate::customize::Downloader;
use crate::error::MdtError;
use crate::mirror::Mirror;
use crate::modloader::ModLoader;
use crate::registry;
use crate::urls::ManifestUrls;
use crate::virtual_dir::VirtualDir;

use super::version_json::{
    download_library, read_vanilla_launch_args, swap_loader_json_in_place, GeneratedLibrary,
    GeneratedVersionJson,
};
use super::{
    dedup_urls, download_to_file_from_candidates, libraries_dir, library_path_from_name,
    loader_version_id, target_version_dir, InstallMethod, InstallRequest, InstalledVersion,
};

pub async fn install(
    downloader: &dyn Downloader,
    req: InstallRequest,
) -> Result<InstalledVersion, MdtError> {
    let urls = ManifestUrls::try_new(&req.mirror)?;
    let id = loader_version_id(&req.mc_version, ModLoader::Liteloader, &req.loader_version);
    let reg_dir = registry::modloader_dir(req.loader.display_name(), &req.loader_version);

    // 1. Download the LiteLoader core JAR into the global cache, then copy into `libraries/`.
    let liteloader_name = format!("com.mumfrey:liteloader:{}", req.loader_version);
    let liteloader_path = library_path_from_name(&liteloader_name)?;
    let liteloader_dest = libraries_dir(&req.root_dir).join(&liteloader_path);
    let cached = reg_dir.join(&liteloader_path);
    let file = format!("{}/liteloader-{}.jar", req.mc_version, req.loader_version);
    let mirrored = urls.apply_mirror(&format!("{}/{}", urls.liteloader_base, file), &req.mirror);
    // Mirror first, official as fallback.
    let official = format!("{}/{}", ManifestUrls::try_new(&Mirror::Mojang)?.liteloader_base, file);
    let candidates = dedup_urls(&[mirrored.clone(), official]);
    if !cached.exists() {
        download_to_file_from_candidates(downloader, &candidates, &cached).await?;
    }
    VirtualDir::new(&cached, &liteloader_dest).mount().await?;
    req.reporter
        .status(&format!("Downloaded liteloader-{}.jar by LiteLoader {}", req.loader_version, req.loader_version));

    // 2. Download LaunchWrapper and ASM (required at runtime).
    let reporter = req.reporter.as_ref();
    download_library(
        downloader,
        "net.minecraft:launchwrapper:1.12",
        "https://libraries.minecraft.net/",
        &req.root_dir,
        &req.mirror,
        &urls,
        reporter,
        None,
        Some(&reg_dir),
    )
    .await?;
    download_library(
        downloader,
        "org.ow2.asm:asm-all:5.2",
        "https://libraries.minecraft.net/",
        &req.root_dir,
        &req.mirror,
        &urls,
        reporter,
        None,
        Some(&reg_dir),
    )
    .await?;

    // 3. Read the vanilla launch args and append the tweak class.
    let (vanilla_args, _) = read_vanilla_launch_args(&req.root_dir, &req.mc_version).await?;
    let mut args = vanilla_args.unwrap_or_else(default_launch_args);
    args.push_str(" --tweakClass com.mumfrey.liteloader.launch.LiteLoaderTweaker");

    // 4. Synthesize the version JSON.
    let json = GeneratedVersionJson {
        id: id.clone(),
        inherits_from: Some(req.mc_version.clone()),
        time: None,
        release_time: None,
        version_type: "release".to_string(),
        main_class: "net.minecraft.launchwrapper.Launch".to_string(),
        minecraft_arguments: Some(args),
        libraries: vec![
            GeneratedLibrary::direct(&liteloader_name, &liteloader_path, &mirrored),
            GeneratedLibrary::maven("net.minecraft:launchwrapper:1.12", "https://libraries.minecraft.net/")?,
            GeneratedLibrary::maven("org.ow2.asm:asm-all:5.2", "https://libraries.minecraft.net/")?,
        ],
    };

    let vdir = target_version_dir(&req, &id);
    tokio::fs::create_dir_all(&vdir).await?;
    let json_path = vdir.join(format!("{id}.json"));
    tokio::fs::write(&json_path, json.to_pretty_json()?).await?;

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

fn default_launch_args() -> String {
    "--username ${auth_player_name} --version ${version_name} --gameDir ${game_directory} \
     --assetsDir ${assets_root} --assetIndex ${assets_index_name} --uuid ${auth_uuid} \
     --accessToken ${auth_access_token} --userType ${user_type} --versionType ${version_type}"
        .to_string()
}
