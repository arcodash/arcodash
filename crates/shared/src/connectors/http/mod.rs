//! HTTP-based connectors (Prometheus, `InfluxDB` v1, Databricks SQL, JSON URL, CSV upload), each in
//! its own sibling module; this module holds only what they share.

use std::fmt::Write;
use std::time::Duration;

use anyhow::{Context, Result};

pub mod csv_upload;
pub mod databricks;
pub mod influxdb;
pub mod json_url;
pub mod prometheus;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared client for the HTTP connectors, with normal DNS resolution. `pub` so the backend's
/// destination test-send can reuse it.
pub fn http_client() -> Result<reqwest::Client> {
    pinned_http_client("", &[])
}

/// Like [`http_client`], but pins DNS for `host` to `addrs`, the addresses
/// [`crate::net_guard::guard_host`] approved, closing the DNS-rebind gap between guard and connect.
///
/// Empty `addrs` (guard bypassed via `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1`) falls back to normal
/// resolution. The hostname stays in the URL, so TLS SNI/cert validation is unaffected.
///
/// Redirects are disabled: a compromised endpoint could otherwise 30x into the cloud metadata IP
/// and bypass the `guard_host` check.
pub fn pinned_http_client(host: &str, addrs: &[std::net::SocketAddr]) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("arcodash/", env!("CARGO_PKG_VERSION")))
        // A blocked host would otherwise hang on the OS TCP connect timeout before `.timeout()`
        // starts counting.
        .connect_timeout(Duration::from_secs(10));
    if !addrs.is_empty() {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    builder.build().context("failed to build HTTP client")
}

/// Classifies a failed `reqwest` request for [`super::connect_err`]: transport-level failures (DNS,
/// connect refused, TLS handshake) tag as `Connect`; anything else (timeout, decode error,
/// post-response error) stays untagged. Shared by the reqwest-based connectors; Athena and MongoDB
/// classify their own error types.
pub(crate) fn classify_reqwest_err(e: reqwest::Error) -> anyhow::Error {
    if e.is_connect() {
        super::connect_err(anyhow::Error::new(e))
    } else {
        anyhow::Error::new(e)
    }
}

/// Percent-encodes `s` over UTF-8 bytes (not `char as u32`) so non-ASCII round-trips; bytes outside
/// the RFC 3986 unreserved set become `%XX`.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Validates a SQL/column identifier: non-empty, only alphanumeric, `_`, `-`, `.`. Shared by
/// connectors that interpolate configured dataset/database names into `INFORMATION_SCHEMA` queries.
pub(crate) fn validate_identifier(s: &str, label: &str) -> Result<()> {
    if s.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    if !s.chars().all(|c| c.is_alphanumeric() || "_-.".contains(c)) {
        anyhow::bail!("{label} contains invalid characters for a SQL identifier");
    }
    Ok(())
}

/// Parses `raw_url` and `guard_host`s its host, returning the host plus the approved addresses for
/// [`pinned_http_client`].
///
/// For connectors storing a full admin-configured URL; `pub` because `data_source_service::create`
/// also calls it.
pub async fn guard_url(raw_url: &str) -> Result<(String, Vec<std::net::SocketAddr>)> {
    guard_url_with(raw_url, crate::net_guard::system_resolve).await
}

/// [`guard_url`] with an injectable resolver (see `net_guard::guard_host_with`).
pub async fn guard_url_with<R>(
    raw_url: &str,
    resolve: R,
) -> Result<(String, Vec<std::net::SocketAddr>)>
where
    R: FnOnce(&str) -> std::io::Result<Vec<std::net::SocketAddr>> + Send + 'static,
{
    let parsed = reqwest::Url::parse(raw_url).context("invalid URL")?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host"))?
        .to_string();
    let addrs = crate::net_guard::guard_host_with(&host, resolve).await?;
    Ok((host, addrs))
}

#[cfg(test)]
mod classify_reqwest_err_tests {
    use super::classify_reqwest_err;

    /// A real transport-level failure (nothing listens on loopback port 1, so the OS refuses at
    /// once) must classify as `Connect`; this covers the logic all reqwest-based connectors share.
    /// Loopback avoids a filtered public host, which would time out instead of being refused.
    #[tokio::test]
    async fn transport_failure_classifies_as_connect() {
        let client = reqwest::Client::builder().build().unwrap();
        let err = client
            .get("http://127.0.0.1:1/")
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .expect_err("nothing listens on 127.0.0.1:1");
        assert!(
            err.is_connect(),
            "expected a transport-level error, got: {err:?}"
        );

        let classified = classify_reqwest_err(err);
        assert_eq!(
            crate::connectors::classify_run_query_error(&classified),
            crate::connectors::ConnectorErrorKind::Connect
        );
    }
}

#[cfg(test)]
mod pinned_http_client_tests {
    use super::pinned_http_client;

    /// Serves one `200 ok` response on a loopback port and returns the port.
    async fn serve_one_ok() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await;
            }
        });
        port
    }

    /// `definitely-not-real.invalid` can never resolve, so a request that still reaches the local
    /// server must have gone straight to the pinned address, not re-resolved.
    #[tokio::test]
    async fn connects_to_the_pinned_address_even_though_the_hostname_cannot_resolve() {
        let port = serve_one_ok().await;
        let fake_host = "definitely-not-real.invalid";
        let pinned = vec![std::net::SocketAddr::from(([127, 0, 0, 1], 0))];
        let client = pinned_http_client(fake_host, &pinned).expect("build pinned client");
        let resp = client
            .get(format!("http://{fake_host}:{port}/"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .expect("pinned client must reach the local server despite the hostname not resolving");
        assert_eq!(resp.status(), 200);
    }

    /// Empty `addrs` (the `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1` bypass case) must fall back to
    /// normal resolution rather than pinning to nothing — a real loopback connection still works.
    #[tokio::test]
    async fn empty_addrs_falls_back_to_normal_resolution() {
        let port = serve_one_ok().await;
        let client = pinned_http_client("127.0.0.1", &[]).expect("build client");
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .expect("unpinned client must still reach loopback");
        assert_eq!(resp.status(), 200);
    }
}

#[cfg(test)]
mod percent_encode_tests {
    use super::percent_encode;

    #[test]
    fn encodes_two_byte_utf8_char_as_its_utf8_bytes() {
        assert_eq!(percent_encode("héllo"), "h%C3%A9llo");
    }

    #[test]
    fn encodes_three_byte_utf8_char_as_its_utf8_bytes() {
        assert_eq!(percent_encode("中"), "%E4%B8%AD");
    }

    #[test]
    fn unreserved_set_is_unchanged() {
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn reserved_ascii_is_percent_encoded() {
        assert_eq!(percent_encode("a b/c?d"), "a%20b%2Fc%3Fd");
    }

    #[test]
    fn non_ascii_output_round_trips_back_to_the_original_bytes() {
        for s in ["héllo", "中文", "café ☕", "Ñoño", "🎉party"] {
            let encoded = percent_encode(s);
            let mut decoded = Vec::new();
            let bytes = encoded.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                    decoded.push(u8::from_str_radix(hex, 16).unwrap());
                    i += 3;
                } else {
                    decoded.push(bytes[i]);
                    i += 1;
                }
            }
            assert_eq!(String::from_utf8(decoded).unwrap(), s);
        }
    }
}

#[cfg(test)]
mod validate_identifier_tests {
    use super::validate_identifier;

    #[test]
    fn empty_string_is_rejected() {
        assert!(validate_identifier("", "db").is_err());
    }

    #[test]
    fn identifier_with_allowed_characters_is_accepted() {
        assert!(validate_identifier("my_db-1.x", "db").is_ok());
    }

    #[test]
    fn identifier_with_a_space_is_rejected() {
        assert!(validate_identifier("a b", "db").is_err());
    }
}
