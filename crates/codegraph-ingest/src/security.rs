//! URL validation + SSRF-guarded fetch (`validate_url` / `safe_fetch`; the URL
//! parts — `sanitize_label` lives in `codegraph-core`).
//!
//! `validate_url` is the SSRF defense: only http/https, no cloud-metadata hosts,
//! and every resolved IP must be public (private/reserved/loopback/link-local/
//! CGN/NAT64 blocked). `safe_fetch` re-validates each redirect hop and caps the
//! response size. (Residual: a DNS rebind between validate and connect; we
//! re-validate per hop, which covers the common cases. Noted, not fully closed.)

use std::net::{IpAddr, ToSocketAddrs};
use std::sync::LazyLock;
use std::time::Duration;

use reqwest::Url;

/// Shared blocking HTTP client (built once, not per `safe_fetch` call), so
/// repeated fetches reuse the connection pool / TLS config. Redirects are
/// disabled here and re-validated manually per hop, so sharing is safe. M3.
static HTTP_CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .user_agent("codegraph-ingest")
        .build()
        .expect("build static reqwest client")
});

/// 50 MB cap for binary downloads.
pub const MAX_FETCH_BYTES: u64 = 52_428_800;
/// 10 MB cap for text/HTML.
pub const MAX_TEXT_BYTES: u64 = 10_485_760;

const BLOCKED_HOSTS: &[&str] = &["metadata.google.internal", "metadata.google.com"];
const MAX_REDIRECTS: usize = 10;

/// Errors from URL validation / fetching.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("invalid URL: {0}")]
    Invalid(String),
    #[error("blocked URL: {0}")]
    Blocked(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("response exceeds the {0}-byte cap")]
    TooLarge(u64),
}

/// True if `ip` is in a private/reserved/internal range — i.e. an SSRF target
/// (incl. RFC 6598 CGN and RFC 6052 NAT64 unwrapping).
fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || o[0] == 0          // 0.0.0.0/8
                || o[0] >= 240        // 240.0.0.0/4 reserved
                || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGN
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18.0.0/15 benchmarking
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24 IETF (incl. NAT64 discovery)
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:0:0/96) and NAT64 (64:ff9b::/96) both embed an
            // IPv4: unwrap and judge the embedded address, so a mapped private/
            // metadata target (e.g. ::ffff:169.254.169.254) can't bypass the V4
            // checks. (Python's is_private treats all of ::ffff:0:0/96 as private.)
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_blocked(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            let is_nat64 =
                seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6].iter().all(|&s| s == 0);
            if is_nat64 {
                let embedded = std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return ip_is_blocked(IpAddr::V4(embedded));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// Validate a URL for fetching: http/https only, not a cloud-metadata host, and
/// every resolved IP public. Returns the parsed [`Url`].
pub fn validate_url(url: &str) -> Result<Url, FetchError> {
    let parsed = Url::parse(url).map_err(|e| FetchError::Invalid(e.to_string()))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(FetchError::Blocked(format!(
            "scheme '{scheme}' not allowed"
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::Invalid("URL has no host".into()))?;
    if BLOCKED_HOSTS.contains(&host.to_lowercase().as_str()) {
        return Err(FetchError::Blocked(format!("cloud-metadata host '{host}'")));
    }
    let port = parsed.port_or_known_default().unwrap_or(80);

    // IP literals are judged directly, before any DNS. The OS resolver's handling
    // of a bracketed IPv6 literal (e.g. `[::1]`, `[::ffff:127.0.0.1]`) differs by
    // platform (Windows resolves it, glibc/macOS reject the brackets), so routing
    // literals through `to_socket_addrs` would let them slip past on some hosts.
    // Parsing the literal ourselves keeps the SSRF check deterministic and offline.
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if ip_is_blocked(ip) {
            return Err(FetchError::Blocked(format!(
                "private/internal IP {ip} (from '{host}')"
            )));
        }
        return Ok(parsed);
    }

    // Otherwise it is a domain name: resolve and reject any private/internal IP.
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| FetchError::Invalid(format!("DNS resolution failed: {e}")))?;
    let mut saw_any = false;
    for a in addrs {
        saw_any = true;
        if ip_is_blocked(a.ip()) {
            return Err(FetchError::Blocked(format!(
                "private/internal IP {} (from '{host}')",
                a.ip()
            )));
        }
    }
    if !saw_any {
        return Err(FetchError::Invalid(format!(
            "host '{host}' did not resolve"
        )));
    }
    Ok(parsed)
}

/// Fetch bytes from a validated URL, re-validating each redirect hop and capping
/// the response at `cap` bytes.
pub fn safe_fetch(url: &str, cap: u64) -> Result<Vec<u8>, FetchError> {
    let client = &*HTTP_CLIENT;

    let mut current = validate_url(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let resp = client
            .get(current.clone())
            .send()
            .map_err(|e| FetchError::Http(e.to_string()))?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| FetchError::Http("redirect without Location".into()))?;
            // Resolve relative redirects against the current URL, then re-validate.
            let next = current
                .join(loc)
                .map_err(|e| FetchError::Invalid(e.to_string()))?;
            current = validate_url(next.as_str())?;
            continue;
        }
        if !status.is_success() {
            return Err(FetchError::Http(format!("status {status}")));
        }
        if let Some(len) = resp.content_length() {
            if len > cap {
                return Err(FetchError::TooLarge(cap));
            }
        }
        // Stream into a bounded buffer: read at most cap+1 bytes so a chunked or
        // dishonest-`Content-Length` response can't exhaust memory before the
        // size check.
        use std::io::Read;
        let mut bytes = Vec::new();
        resp.take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| FetchError::Http(e.to_string()))?;
        if bytes.len() as u64 > cap {
            return Err(FetchError::TooLarge(cap));
        }
        return Ok(bytes);
    }
    Err(FetchError::Http("too many redirects".into()))
}

/// Fetch text from a validated URL (10 MB cap), lossily decoding as UTF-8.
pub fn safe_fetch_text(url: &str) -> Result<String, FetchError> {
    let bytes = safe_fetch(url, MAX_TEXT_BYTES)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_schemes() {
        assert!(matches!(
            validate_url("file:///etc/passwd"),
            Err(FetchError::Blocked(_))
        ));
        assert!(matches!(
            validate_url("ftp://example.com/x"),
            Err(FetchError::Blocked(_))
        ));
    }

    #[test]
    fn blocks_metadata_host() {
        assert!(matches!(
            validate_url("http://metadata.google.internal/"),
            Err(FetchError::Blocked(_))
        ));
    }

    #[test]
    fn blocks_private_and_loopback_ip_literals() {
        // IP literals resolve without network, so this is offline-testable.
        for u in [
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/", // AWS metadata
            "http://100.64.0.1/",      // CGN
            "http://[::1]/",
        ] {
            assert!(
                matches!(validate_url(u), Err(FetchError::Blocked(_))),
                "should block {u}"
            );
        }
    }

    #[test]
    fn allows_a_public_ip_literal() {
        // 8.8.8.8 is public, so it passes the IP check (no network fetch here).
        assert!(validate_url("http://8.8.8.8/").is_ok());
    }

    #[test]
    fn blocks_ipv4_mapped_ipv6_internal_targets() {
        // ::ffff:a.b.c.d must be judged on the embedded IPv4 (SSRF bypass guard).
        for u in [
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:169.254.169.254]/", // mapped AWS metadata
            "http://[::ffff:10.0.0.1]/",
        ] {
            assert!(
                matches!(validate_url(u), Err(FetchError::Blocked(_))),
                "should block {u}"
            );
        }
    }

    #[test]
    fn blocks_ietf_and_benchmarking_ranges() {
        for u in [
            "http://192.0.0.1/",   // 192.0.0.0/24 IETF protocol assignments
            "http://192.0.0.170/", // RFC 7050 NAT64/DNS64 discovery
            "http://198.18.0.1/",  // 198.18.0.0/15 benchmarking
            "http://198.19.255.1/",
        ] {
            assert!(
                matches!(validate_url(u), Err(FetchError::Blocked(_))),
                "should block {u}"
            );
        }
    }
}
