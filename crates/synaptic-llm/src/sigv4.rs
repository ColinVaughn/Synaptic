//! Minimal AWS Signature Version 4 signing — just enough to call the Bedrock
//! Converse endpoint without pulling the full AWS SDK. The HMAC chain and
//! canonical-request construction are validated in tests against AWS's published
//! SigV4 example vectors (see `tests`), so the cryptographic core is known-good.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// AWS credentials. `session_token` is set for temporary (STS/role) credentials.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

/// Lowercase hex of the SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// Derive the SigV4 signing key (`HMAC` chain over date → region → service →
/// `aws4_request`).
fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// Final signature (lowercase hex) for a `string_to_sign`. `date_stamp` is the
/// `YYYYMMDD` form. Validated against AWS's documented derivation example.
pub fn derive_signature(
    secret: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
    string_to_sign: &str,
) -> String {
    let key = signing_key(secret, date_stamp, region, service);
    to_hex(&hmac(&key, string_to_sign.as_bytes()))
}

/// RFC 3986 path encoding (single-encode, '/' preserved) for the canonical URI.
/// Bedrock model ids contain `:` and `.`, which must be `%`-encoded except `.`
/// (unreserved).
pub fn uri_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the SigV4 `Authorization` header value for a request.
///
/// `headers` are the headers to sign (must include `host` and `x-amz-date`; for
/// a POST add `content-type`, and `x-amz-security-token` when using temporary
/// credentials). `amz_date` is the full `YYYYMMDDTHHMMSSZ` timestamp.
#[allow(clippy::too_many_arguments)]
pub fn authorization_header(
    method: &str,
    canonical_uri: &str,
    canonical_querystring: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    creds: &Credentials,
    amz_date: &str,
    region: &str,
    service: &str,
) -> String {
    // Canonical headers: lowercase name, trimmed value, sorted by name, each
    // terminated by '\n'. signed_headers: the names, ';'-joined.
    let mut pairs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = pairs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers: String = pairs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_querystring}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let date_stamp = &amz_date[..8];
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signature = derive_signature(
        &creds.secret_key,
        date_stamp,
        region,
        service,
        &string_to_sign,
    );
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS-published constants used across the SigV4 documentation examples.
    const AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const AWS_ACCESS: &str = "AKIDEXAMPLE";

    #[test]
    fn sha256_hex_of_empty_string_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn signing_key_and_signature_match_aws_derivation_example() {
        // From AWS docs "Examples of how to derive a signing key for SigV4":
        // service=iam, region=us-east-1, date=20150830, with the canonical
        // string-to-sign below, the documented signature is fixed.
        let string_to_sign = "AWS4-HMAC-SHA256\n\
            20150830T123600Z\n\
            20150830/us-east-1/iam/aws4_request\n\
            f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59";
        let sig = derive_signature(AWS_SECRET, "20150830", "us-east-1", "iam", string_to_sign);
        assert_eq!(
            sig,
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn authorization_header_matches_aws_sigv4_get_vanilla_vector() {
        // AWS SigV4 test-suite `get-vanilla`: a GET to example.amazonaws.com with
        // only host + x-amz-date, service=service, region=us-east-1. The expected
        // Authorization is published and fixed, validating the whole algorithm
        // (canonical request -> string to sign -> signing key -> signature).
        let creds = Credentials {
            access_key: AWS_ACCESS.into(),
            secret_key: AWS_SECRET.into(),
            session_token: None,
        };
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let auth = authorization_header(
            "GET",
            "/",
            "",
            &headers,
            &sha256_hex(b""),
            &creds,
            "20150830T123600Z",
            "us-east-1",
            "service",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn uri_encode_path_encodes_bedrock_model_colon() {
        // The Converse path embeds a model id with a ':' that must be %-encoded
        // (but '.' and '-' are unreserved and kept).
        assert_eq!(
            uri_encode_path("/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse"),
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse"
        );
    }
}
