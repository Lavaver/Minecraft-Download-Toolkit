//! Quilt loader installation (Method B: manual unpack).
//!
//! Quilt ships only its core components; MDT fetches the official profile JSON, downloads its
//! dependencies, and synthesizes the version JSON — no Java required. Under [`crate::Mirror::BMCLAPI`]
//! the profile and Maven URLs fall back to the official Quilt servers (BMCLAPI does not proxy them).

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
    let mirrored = urls.quilt_profile_url(&req.loader_version, &req.mc_version, &req.mirror);
    // Mirror first, official as fallback.
    let official =
        ManifestUrls::try_new(&Mirror::Mojang)?.quilt_profile(&req.loader_version, &req.mc_version);
    let candidates = dedup_urls(&[mirrored, official]);
    install_from_profile(downloader, &candidates, &req, &urls).await
}
