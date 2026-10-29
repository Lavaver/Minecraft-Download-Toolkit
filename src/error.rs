//! The one error type for the whole crate.
//!
//! Anything that can fail returns [`Result<T, MdtError>`][std::result::Result], so you can match
//! on the exact problem instead of a boxed catch-all.

use thiserror::Error;

/// The error every MDT call can return.
///
/// Two kinds of variants:
///
/// * **Passthroughs** (`#[from]`) for other libraries' errors — network, JSON, XML, I/O, ZIP,
///   and task joins. These make `?` work directly.
/// * **MDT's own failures** — version not found, checksum mismatch, loader incompatibility, etc.
///
/// [`MdtError::context`] wraps any variant with extra context, like `anyhow` but still a normal,
/// matchable enum.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum MdtError {
    /// An HTTP request failed (DNS, TLS, status code, streaming, ...).
    #[error("network error: {0}")]
    NetworkError(#[from] reqwest::Error),

    /// A payload could not be deserialized as JSON.
    #[error("JSON parse error: {0}")]
    JsonParseError(#[from] serde_json::Error),

    /// A payload could not be deserialized as XML (Maven metadata).
    #[error("XML parse error: {0}")]
    XmlParseError(#[from] quick_xml::de::DeError),

    /// A filesystem operation failed.
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// A ZIP archive could not be opened or extracted.
    #[error("ZIP error: {0}")]
    ZipError(#[from] zip::result::ZipError),

    /// A background `spawn_blocking` task panicked or was cancelled.
    #[error("background task failed: {0}")]
    TaskPanic(#[from] tokio::task::JoinError),

    /// An error wrapped with additional human-readable context.
    #[error("{context}: {source}")]
    Context {
        /// A description of where the failure occurred.
        context: String,
        /// The underlying error.
        #[source]
        source: Box<MdtError>,
    },

    /// The requested Minecraft version does not exist in the version manifest.
    #[error("version not found: {0}")]
    VersionNotFound(String),

    /// A mod loader could not be installed.
    #[error("loader install failed: {0}")]
    LoaderInstallFailed(String),

    /// No Java runtime was found on this machine.
    #[error("Java runtime not found: install Java 8+ or 17+ and retry")]
    JavaNotFound,

    /// A downloaded file failed its SHA-1 integrity check.
    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The destination path of the corrupted file.
        path: String,
        /// The expected SHA-1 (from the manifest).
        expected: String,
        /// The SHA-1 actually computed over the downloaded bytes.
        actual: String,
    },

    /// A manifest or version document was structurally invalid.
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    /// A mirror provider name is not declared in `providers.compile.toml`.
    #[error("unknown mirror provider: {0}")]
    UnknownProvider(String),

    /// Uninstalling a mod loader failed (e.g. post-restore verification).
    #[error("uninstall failed: {0}")]
    UninstallFailed(String),

    /// Some downloads failed after exhausting retries; the remaining queue still completed.
    #[error("partial download failure: {0:?}")]
    PartialDownloadFailed(Vec<String>),

    /// A virtual mount (hardlink / symlink / copy) failed to materialize a file.
    #[error("virtual mount failed: {0}")]
    VirtualDirMountFailed(String),

    /// The disk ran out of space while writing a file.
    #[error("{0}")]
    DiskFull(String),
}

impl MdtError {
    /// Wrap this error with human-readable context, returning [`MdtError::Context`].
    ///
    /// # Example
    ///
    /// ```
    /// use mdt::MdtError;
    ///
    /// let wrapped = MdtError::JavaNotFound.context("installing Forge 1.20.1");
    /// assert_eq!(
    ///     wrapped.to_string(),
    ///     "installing Forge 1.20.1: Java runtime not found: install Java 8+ or 17+ and retry"
    /// );
    /// ```
    #[must_use]
    pub fn context(self, context: impl Into<String>) -> Self {
        MdtError::Context {
            context: context.into(),
            source: Box::new(self),
        }
    }
}

/// Whether an I/O error indicates the disk is full.
///
/// Windows reports `ERROR_HANDLE_DISK_FULL` (39) or `ERROR_DISK_FULL` (112); POSIX reports
/// `ENOSPC` (28).
pub(crate) fn is_disk_full(e: &std::io::Error) -> bool {
    let Some(code) = e.raw_os_error() else {
        return false;
    };
    if cfg!(windows) {
        code == 39 || code == 112
    } else if cfg!(unix) {
        code == 28
    } else {
        false
    }
}

/// Convert an I/O error into an [`MdtError`], promoting disk-full errors to [`MdtError::DiskFull`].
pub(crate) fn io_error(e: std::io::Error, context: impl Into<String>) -> MdtError {
    let context = context.into();
    if is_disk_full(&e) {
        MdtError::DiskFull(format!("disk space exhausted {context}"))
    } else {
        MdtError::IoError(e).context(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_wraps_with_human_readable_message() {
        let wrapped = MdtError::JavaNotFound.context("installing Forge 1.20.1");
        assert_eq!(
            wrapped.to_string(),
            "installing Forge 1.20.1: Java runtime not found: install Java 8+ or 17+ and retry"
        );
    }

    #[test]
    fn checksum_mismatch_displays_path_and_hashes() {
        let err = MdtError::ChecksumMismatch {
            path: "libraries/x.jar".to_string(),
            expected: "abc".to_string(),
            actual: "def".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "checksum mismatch for libraries/x.jar: expected abc, got def"
        );
    }

    #[test]
    fn partial_download_failed_lists_urls() {
        let err = MdtError::PartialDownloadFailed(vec!["https://a".to_string(), "https://b".to_string()]);
        let s = err.to_string();
        assert!(s.contains("partial download failure"), "{s}");
        assert!(s.contains("https://a") && s.contains("https://b"), "{s}");
    }

    #[test]
    fn is_disk_full_detects_platform_codes() {
        let code39 = std::io::Error::from_raw_os_error(39);
        let code112 = std::io::Error::from_raw_os_error(112);
        let code28 = std::io::Error::from_raw_os_error(28);

        #[cfg(windows)]
        {
            assert!(is_disk_full(&code39));
            assert!(is_disk_full(&code112));
            assert!(!is_disk_full(&code28));
        }
        #[cfg(unix)]
        {
            assert!(is_disk_full(&code28));
            assert!(!is_disk_full(&code39));
            assert!(!is_disk_full(&code112));
        }
    }
}
