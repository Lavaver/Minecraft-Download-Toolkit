//! Download mirror sources.

/// Where game files and metadata come from.
///
/// [`Mirror::BMCLAPI`] is the default and usually faster in mainland China; [`Mirror::Mojang`]
/// is the official source. When a request against the active mirror fails, MDT retries against
/// [`Mirror::other`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Mirror {
    /// Official Mojang servers.
    Mojang,
    /// The BMCLAPI community mirror (proxies Mojang, Fabric, Forge, NeoForge, and LiteLoader).
    #[default]
    BMCLAPI,
    /// A custom mirror provider, keyed by its `[provider_name]` section in `providers.compile.toml`.
    ///
    /// The name must match a section compiled in at build time — see the
    /// [TOML spec](/docs/mirror_toml.md).
    Custom(String),
}

impl Mirror {
    /// The opposite mirror, used as an automatic fallback when the primary source fails.
    #[must_use]
    pub fn other(&self) -> Self {
        match self {
            Mirror::Mojang => Mirror::BMCLAPI,
            Mirror::BMCLAPI => Mirror::Mojang,
            // A custom mirror has no single "opposite"; fall back to the default mirror.
            Mirror::Custom(_) => Mirror::BMCLAPI,
        }
    }

    /// The provider name from `providers.compile.toml`.
    ///
    /// Used to look up that provider's endpoints and rewrite rules (see
    /// [`crate::urls::ManifestUrls`]). Built-ins return `"mojang"` / `"bmclapi"`;
    /// [`Mirror::Custom`] returns its own name.
    ///
    /// - URI spec: <https://github.com/Lavaver/MDT/blob/main/docs/mirror_uri.md>
    /// - TOML spec: <https://github.com/Lavaver/MDT/blob/main/docs/mirror_toml.md>
    #[must_use]
    pub fn provider_name(&self) -> &str {
        match self {
            Mirror::Mojang => "mojang",
            Mirror::BMCLAPI => "bmclapi",
            Mirror::Custom(name) => name.as_str(),
        }
    }

    /// A human-friendly label for progress output, e.g. `Mojang` or `BMCLAPI`.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Mirror::Mojang => "Mojang",
            Mirror::BMCLAPI => "BMCLAPI",
            Mirror::Custom(name) => name.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Mirror;

    #[test]
    fn label_is_human_friendly() {
        assert_eq!(Mirror::Mojang.label(), "Mojang");
        assert_eq!(Mirror::BMCLAPI.label(), "BMCLAPI");
        assert_eq!(Mirror::Custom("my_mirror".into()).label(), "my_mirror");
    }

    #[test]
    fn other_falls_back_to_builtin() {
        assert_eq!(Mirror::Mojang.other(), Mirror::BMCLAPI);
        assert_eq!(Mirror::BMCLAPI.other(), Mirror::Mojang);
        assert_eq!(Mirror::Custom("x".into()).other(), Mirror::BMCLAPI);
    }
}
