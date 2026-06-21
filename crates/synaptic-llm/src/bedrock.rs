//! AWS Bedrock backend via the Converse API, SigV4-signed with the in-tree
//! [`crate::sigv4`] signer (no AWS SDK dependency).
//!
//! Credentials come from the standard AWS environment variables
//! (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`); profile
//! resolution (`~/.aws/credentials`) is out of scope. The signing math is
//! validated against AWS test vectors in
//! [`crate::sigv4`]; this module is additionally exercised against a mock server
//! for request shape and response parsing.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::LlmError;
use crate::provider::{Completion, LlmClient};
use crate::sigv4::{authorization_header, sha256_hex, uri_encode_path, Credentials};

/// Default Bedrock model; override with `BEDROCK_MODEL`.
pub const BEDROCK_DEFAULT_MODEL: &str = "anthropic.claude-3-5-sonnet-20241022-v2:0";
const SERVICE: &str = "bedrock";

/// Bedrock Converse client.
pub struct Bedrock {
    endpoint: String,
    region: String,
    creds: Credentials,
    model: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl Bedrock {
    /// Build a client for `region` with `creds` and `model`. The endpoint
    /// defaults to the public `bedrock-runtime` host for the region.
    pub fn new(region: impl Into<String>, creds: Credentials, model: impl Into<String>) -> Self {
        let region = region.into();
        let endpoint = format!("https://bedrock-runtime.{region}.amazonaws.com");
        Bedrock {
            endpoint,
            region,
            creds,
            model: model.into(),
            max_tokens: 8192,
            client: reqwest::Client::new(),
        }
    }

    /// Override the endpoint (used by tests to point at a mock server).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    async fn send(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        // Serialize the body ourselves so the bytes we SigV4-hash are exactly the
        // bytes we send (reqwest's `.json()` re-serializes and could differ).
        let body = converse_body(system, user, self.max_tokens).to_string();
        let payload_hash = sha256_hex(body.as_bytes());

        // Build the URL with the model id %-encoded the same way it's signed, so
        // the wire path matches the canonical URI.
        let canonical_path = uri_encode_path(&format!("/model/{}/converse", self.model));
        let url_str = format!("{}{canonical_path}", self.endpoint.trim_end_matches('/'));
        let url = reqwest::Url::parse(&url_str)
            .map_err(|e| LlmError::BadResponse(format!("bad bedrock url {url_str}: {e}")))?;

        let host = host_header(&url);
        let amz_date = amz_now();

        // Headers to sign (and send). Order here is irrelevant; the signer sorts.
        let mut signed: Vec<(&str, &str)> = vec![
            ("content-type", "application/json"),
            ("host", host.as_str()),
            ("x-amz-date", amz_date.as_str()),
        ];
        if let Some(tok) = self.creds.session_token.as_deref() {
            signed.push(("x-amz-security-token", tok));
        }
        let authorization = authorization_header(
            "POST",
            &canonical_path,
            "",
            &signed,
            &payload_hash,
            &self.creds,
            &amz_date,
            &self.region,
            SERVICE,
        );

        let mut req = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("x-amz-date", &amz_date)
            .header("authorization", authorization)
            .body(body);
        if let Some(tok) = self.creds.session_token.as_deref() {
            req = req.header("x-amz-security-token", tok);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let v: Value = resp.json().await?;
        Ok(parse_converse(&v))
    }
}

#[async_trait]
impl LlmClient for Bedrock {
    async fn complete(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        self.send(system, user).await
    }
}

/// Parse a Bedrock Converse response into a [`Completion`].
fn parse_converse(v: &Value) -> Completion {
    let content = v
        .get("output")
        .and_then(|o| o.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|b| b.get("text").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let usage = v.get("usage");
    let tok = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };
    let finish_reason = match v.get("stopReason").and_then(Value::as_str) {
        Some("max_tokens") => "length",
        _ => "stop",
    }
    .to_string();
    Completion {
        content,
        finish_reason,
        input_tokens: tok("inputTokens"),
        output_tokens: tok("outputTokens"),
    }
}

/// The Converse request body for a single user turn.
fn converse_body(system: &str, user: &str, max_tokens: u32) -> Value {
    json!({
        "system": [{"text": system}],
        "messages": [{"role": "user", "content": [{"text": user}]}],
        "inferenceConfig": {"maxTokens": max_tokens, "temperature": 0},
    })
}

/// Current time as a SigV4 `YYYYMMDDTHHMMSSZ` timestamp.
fn amz_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_amz_date(secs)
}

/// Format epoch seconds as a SigV4 UTC timestamp `YYYYMMDDTHHMMSSZ`.
fn format_amz_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// Convert days-since-Unix-epoch to a `(year, month, day)` civil date
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The `Host` header value reqwest will send for `url` (includes the port only
/// when it is non-default for the scheme).
fn host_header(url: &reqwest::Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        (None, _) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn creds() -> Credentials {
        Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    #[test]
    fn format_amz_date_matches_known_epochs() {
        assert_eq!(format_amz_date(0), "19700101T000000Z");
        // 1234567890 is the well-known 2009-02-13 23:31:30 UTC.
        assert_eq!(format_amz_date(1_234_567_890), "20090213T233130Z");
    }

    #[test]
    fn converse_body_has_system_and_user_turn() {
        let b = converse_body("sys", "usr", 1234);
        assert_eq!(b["system"][0]["text"], json!("sys"));
        assert_eq!(b["messages"][0]["content"][0]["text"], json!("usr"));
        assert_eq!(b["inferenceConfig"]["maxTokens"], json!(1234));
    }

    #[test]
    fn parse_converse_extracts_text_usage_and_stop_reason() {
        let v = json!({
            "output": {"message": {"content": [{"text": "{\"nodes\": []}"}]}},
            "usage": {"inputTokens": 9, "outputTokens": 2},
            "stopReason": "end_turn"
        });
        let c = parse_converse(&v);
        assert_eq!(c.content, "{\"nodes\": []}");
        assert_eq!(c.input_tokens, 9);
        assert_eq!(c.output_tokens, 2);
        assert_eq!(c.finish_reason, "stop");
    }

    #[test]
    fn parse_converse_maps_max_tokens_to_length() {
        let v = json!({
            "output": {"message": {"content": [{"text": "x"}]}},
            "stopReason": "max_tokens"
        });
        assert_eq!(parse_converse(&v).finish_reason, "length");
    }

    #[tokio::test]
    async fn signs_and_parses_a_converse_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model/test-model/converse"))
            .and(header_exists("authorization"))
            .and(header_exists("x-amz-date"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "output": {"message": {"content": [{"text": "{\"nodes\": []}"}]}},
                "usage": {"inputTokens": 5, "outputTokens": 1},
                "stopReason": "end_turn"
            })))
            .mount(&server)
            .await;

        let client = Bedrock::new("us-east-1", creds(), "test-model").with_endpoint(server.uri());
        let c = client.complete("system", "user").await.unwrap();
        assert_eq!(c.content, "{\"nodes\": []}");
        assert_eq!(c.input_tokens, 5);

        // The Authorization header must be a SigV4 credential for the bedrock service.
        let reqs = server.received_requests().await.unwrap();
        let auth = reqs[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
    }

    #[tokio::test]
    async fn temporary_credentials_add_the_security_token_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model/test-model/converse"))
            .and(header_exists("x-amz-security-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "output": {"message": {"content": [{"text": "{}"}]}},
                "usage": {"inputTokens": 1, "outputTokens": 1},
                "stopReason": "end_turn"
            })))
            .mount(&server)
            .await;
        let temp = Credentials {
            session_token: Some("FwoSESSIONTOKEN".into()),
            ..creds()
        };
        let client = Bedrock::new("us-east-1", temp, "test-model").with_endpoint(server.uri());
        client.complete("s", "u").await.unwrap();
    }

    #[tokio::test]
    async fn error_status_surfaces_body_for_overflow_detection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("prompt is too long: 250000 tokens > 200000 maximum"),
            )
            .mount(&server)
            .await;
        let client = Bedrock::new("us-east-1", creds(), "test-model").with_endpoint(server.uri());
        let err = client.complete("s", "u").await.unwrap_err();
        assert!(err.is_context_overflow(), "overflow detected: {err}");
    }
}
