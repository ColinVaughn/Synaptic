//! Native Anthropic Messages API backend (`POST /v1/messages` with an
//! `x-api-key` header), distinct from the OpenAI-compatible layer. Anthropic uses
//! a different request/response shape than OpenAI: a top-level `system` field, a
//! `content` block array in the reply, and `usage.{input,output}_tokens` /
//! `stop_reason` rather than the OpenAI vocab.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::LlmError;
use crate::provider::{Completion, LlmClient};

/// Default model for the native Anthropic backend (`claude-sonnet-4-6`);
/// override with `ANTHROPIC_MODEL`.
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-sonnet-4-6";
/// `anthropic-version` header value (the stable Messages API version).
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages API client.
pub struct Anthropic {
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    version: String,
    client: reqwest::Client,
}

impl Anthropic {
    /// `base_url` is the API root (e.g. `https://api.anthropic.com`); the
    /// `/v1/messages` path is appended.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Anthropic {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 8192,
            version: ANTHROPIC_VERSION.to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    fn endpoint_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    /// Post one Messages request. `user_content` is a plain string for text or a
    /// structured block array for multimodal (vision) requests.
    async fn send(&self, system: &str, user_content: Value) -> Result<Completion, LlmError> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": user_content}],
        });
        let resp = self
            .client
            .post(self.endpoint_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body,
            });
        }

        let v: Value = resp.json().await?;
        // The reply `content` is a block array; take the first text block.
        let content = v
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Normalize Anthropic's `stop_reason` to the OpenAI-compat `finish_reason`
        // vocabulary so the adaptive-retry layer is backend-agnostic.
        let finish_reason = match v.get("stop_reason").and_then(Value::as_str) {
            Some("max_tokens") => "length",
            _ => "stop",
        }
        .to_string();
        let usage = v.get("usage");
        let tok = |k: &str| {
            usage
                .and_then(|u| u.get(k))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32
        };
        Ok(Completion {
            content,
            finish_reason,
            input_tokens: tok("input_tokens"),
            output_tokens: tok("output_tokens"),
        })
    }
}

#[async_trait]
impl LlmClient for Anthropic {
    async fn complete(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        self.send(system, Value::String(user.to_string())).await
    }

    async fn complete_with_content(
        &self,
        system: &str,
        user: &Value,
    ) -> Result<Completion, LlmError> {
        self.send(system, user.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn parses_a_successful_messages_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "{\"nodes\": []}"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 11, "output_tokens": 4}
            })))
            .mount(&server)
            .await;

        let client = Anthropic::new(server.uri(), "sk-ant", "claude-sonnet-4-6");
        let c = client.complete("system", "user").await.unwrap();
        assert_eq!(c.content, "{\"nodes\": []}");
        assert_eq!(c.finish_reason, "stop");
        assert_eq!(c.input_tokens, 11);
        assert_eq!(c.output_tokens, 4);
        assert!(!c.is_truncated());
    }

    async fn first_body(server: &MockServer) -> Value {
        let reqs = server.received_requests().await.unwrap();
        serde_json::from_slice(&reqs[0].body).unwrap()
    }

    #[tokio::test]
    async fn sends_system_at_top_level_and_user_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "{}"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&server)
            .await;
        let client = Anthropic::new(server.uri(), "k", "m");
        client.complete("be terse", "hello").await.unwrap();
        let body = first_body(&server).await;
        // Anthropic puts the system prompt at the top level, NOT as a message.
        assert_eq!(body["system"], json!("be terse"));
        assert_eq!(body["messages"][0]["role"], json!("user"));
        assert_eq!(body["messages"][0]["content"], json!("hello"));
        assert!(body.get("max_tokens").is_some());
    }

    #[tokio::test]
    async fn max_tokens_stop_reason_maps_to_length() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "{\"nodes\": [partial"}],
                "stop_reason": "max_tokens",
                "usage": {"input_tokens": 1, "output_tokens": 8}
            })))
            .mount(&server)
            .await;
        let client = Anthropic::new(server.uri(), "k", "m");
        let c = client.complete("s", "u").await.unwrap();
        assert_eq!(c.finish_reason, "length");
        assert!(c.is_truncated());
    }

    #[tokio::test]
    async fn error_status_surfaces_body_for_overflow_detection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("prompt is too long: 250000 tokens > 200000 maximum"),
            )
            .mount(&server)
            .await;
        let client = Anthropic::new(server.uri(), "k", "m");
        let err = client.complete("s", "u").await.unwrap_err();
        assert!(err.is_context_overflow(), "should detect overflow: {err}");
    }

    #[tokio::test]
    async fn complete_with_content_transmits_image_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "{}"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&server)
            .await;
        let client = Anthropic::new(server.uri(), "k", "claude-sonnet-4-6");
        // The block shape `vision::anthropic_content` produces for an image.
        let content = json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
            {"type": "text", "text": "describe"}
        ]);
        client.complete_with_content("s", &content).await.unwrap();
        let body = first_body(&server).await;
        let user_content = &body["messages"][0]["content"];
        assert!(
            user_content.is_array(),
            "blocks sent verbatim: {user_content}"
        );
        assert_eq!(user_content[0]["type"], json!("image"));
        assert_eq!(user_content[0]["source"]["media_type"], json!("image/png"));
    }
}
