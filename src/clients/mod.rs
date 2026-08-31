pub mod chat_completion;
pub mod github;
pub mod linear;
pub mod openrouter_pricing;
pub mod telegram;

/// Raw HTTP response at the IO/pure boundary.
///
/// The effectful layer (closure) produces this; the pure layer
/// (`interpret_response`) consumes it. No `reqwest` types leak across.
pub struct RawResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// Seconds from the `Retry-After` header, when the server sent one
    /// (rate-limit responses). `None` when absent or not plumbed.
    pub retry_after_secs: Option<u64>,
}

/// Build the HTTP client for the closure IO layer. `mock-network`
/// builds skip loading system CA certificates: the nix sandbox has
/// none, and loopback fixture servers are plain HTTP.
pub(crate) fn http_client(builder: reqwest::ClientBuilder) -> reqwest::Client {
    #[cfg(feature = "mock-network")]
    let builder = builder.tls_certs_only(Vec::<reqwest::Certificate>::new());
    #[allow(clippy::expect_used)]
    // TLS backend init; fails only on a broken host, at first client build
    builder.build().expect("failed to build HTTP client")
}

/// Refuse non-loopback base URLs — `mock-network` builds must never
/// reach the real network, only local fixture servers.
#[cfg(feature = "mock-network")]
pub(crate) fn assert_loopback(url: &str) {
    assert!(
        is_loopback(url),
        "mock-network build refuses non-loopback URL: {url}"
    );
}

/// True when the URL's host is loopback (`localhost`, `127.0.0.1`, `[::1]`).
#[cfg(any(test, feature = "mock-network"))]
fn is_loopback(url: &str) -> bool {
    let Some(host) = url_host(url) else {
        return false;
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Host component of a URL: `http://[::1]:8080/x` → `::1`.
#[cfg(any(test, feature = "mock-network"))]
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next(),
        None => authority.split(':').next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_accepted() {
        for url in [
            "http://localhost:8080",
            "http://127.0.0.1:8080/path",
            "http://127.1.2.3",
            "http://[::1]:9000/graphql",
        ] {
            assert!(is_loopback(url), "{url} should be loopback");
        }
    }

    #[test]
    fn remote_hosts_rejected() {
        for url in [
            "https://api.telegram.org",
            "https://api.linear.app/graphql",
            "https://openrouter.ai/api/v1",
            "http://192.168.1.1:8080",
            "http://[2001:db8::1]/x",
            "not a url",
            "",
        ] {
            assert!(!is_loopback(url), "{url} should not be loopback");
        }
    }
}
