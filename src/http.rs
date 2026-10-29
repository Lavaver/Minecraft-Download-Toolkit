//! Shared HTTP client construction.

use std::time::Duration;

use crate::error::MdtError;

/// A Chrome-like `User-Agent`, used to avoid being blocked by mirror CDNs and WAFs that
/// reject requesters without a browser UA.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Build the shared HTTP client used by [`crate::MdtClient`].
///
/// Cheap to clone (`Arc` under the hood) and reuses one connection pool across all downloads,
/// so you're not doing a TLS handshake per file.
///
/// reqwest negotiates HTTP/2 via ALPN and transparently falls back to HTTP/1.1 when the server
/// doesn't speak it. The keep-alive pool is deliberately generous (90s idle, 20 connections per
/// host) so a burst of asset downloads reuses warm connections.
///
/// Timeouts protect against a stalled mirror without killing slow-but-healthy downloads: the
/// connect phase is capped by `connect_timeout`, and `read_timeout` aborts only when no bytes
/// arrive for that long (it resets after every successful read). There is deliberately no *total*
/// request timeout, since large files legitimately take longer than any fixed wall-clock bound.
pub fn build_client(
    proxy: Option<&str>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<reqwest::Client, MdtError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(20)
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout);

    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_succeeds() {
        let _client = build_client(None, Duration::from_secs(15), Duration::from_secs(30)).unwrap();
    }

    #[test]
    fn build_client_succeeds_with_proxy() {
        let _client = build_client(
            Some("http://127.0.0.1:1"),
            Duration::from_secs(15),
            Duration::from_secs(30),
        )
        .unwrap();
    }

    #[test]
    fn user_agent_is_browser_like() {
        assert!(USER_AGENT.contains("Mozilla"), "missing Mozilla token");
        assert!(USER_AGENT.contains("Chrome"), "missing Chrome token");
    }
}
