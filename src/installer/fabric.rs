//! Fabric loader installation (Method B: manual unpack).
//!
//! Fabric ships only its core components (no official installer); MDT fetches the official
//! profile JSON, downloads its dependencies, and synthesizes the version JSON — no Java required.

use crate::customize::Downloader;
use crate::error::MdtError;
use crate::mirror::Mirror;
use crate::urls::ManifestUrls;

use super::version_json::install_from_profile;
use super::{dedup_urls, InstallRequest, InstalledVersion};

pub async fn install(
    downloader: &dyn Downloader,
    req: InstallRequest,
) -> Result<InstalledVersion, MdtError> {
    let urls = ManifestUrls::try_new(&req.mirror)?;
    let mirrored = urls.fabric_profile(&req.loader_version, &req.mc_version);
    // Mirror first, official as fallback.
    let official =
        ManifestUrls::try_new(&Mirror::Mojang)?.fabric_profile(&req.loader_version, &req.mc_version);
    let candidates = dedup_urls(&[mirrored, official]);
    install_from_profile(downloader, &candidates, &req, &urls).await
}
