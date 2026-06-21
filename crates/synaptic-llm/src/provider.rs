//! The LLM client abstraction and an OpenAI-compatible backend.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::LlmError;

/// One model reply.
#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    /// `"stop"`, `"length"` (truncated), or provider-specific.
    pub finish_reason: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl Completion {
    /// True when the reply was cut off (hit `max_tokens`) or is hollow (HTTP 200
    /// but empty content) — both signal "retry on a smaller input".
    pub fn is_truncated(&self) -> bool {
        self.finish_reason == "length" || self.content.trim().is_empty()
    }
}

/// A chat-completion provider. Backends implement this; the extraction layer is
/// generic over it (and tests substitute a mock).
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<Completion, LlmError>;

    /// Send a user message whose `content` may be structured — e.g. the multimodal
    /// image payload built by `vision::openai_content`/`anthropic_content`. The
    /// default flattens to text via [`complete`](Self::complete) so non-vision
    /// backends still work; vision-capable backends override this to transmit the
    /// structured content verbatim (otherwise image parts never reach the model).
    async fn complete_with_content(
        &self,
        system: &str,
        user: &Value,
    ) -> Result<Completion, LlmError> {
        let text = user
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| user.to_string());
        self.complete(system, &text).await
    }

    /// Like [`complete`](Self::complete) but requests a streamed (SSE) response
    /// and accumulates it into the final [`Completion`]. Streaming keeps a
    /// long-running request alive (avoids idle-connection timeouts on large
    /// outputs). The default delegates to [`complete`](Self::complete); backends
    /// that support server-sent events override this.
    async fn complete_streaming(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        self.complete(system, user).await
    }
}

/// How a backend addresses the chat-completions endpoint and authenticates.
#[derive(Debug, Clone)]
pub enum ApiStyle {
    /// `{base}/chat/completions` with `Authorization: Bearer {key}` (OpenAI,
    /// DeepSeek, Kimi, Gemini-compat, Ollama, custom providers).
    OpenAi,
    /// Azure OpenAI Service: `{endpoint}/openai/deployments/{deployment}/chat/
    /// completions?api-version={ver}` with an `api-key` header (the deployment is
    /// the `model`).
    Azure { api_version: String },
}

/// OpenAI-compatible Chat Completions backend (covers OpenAI, DeepSeek, Kimi,
/// Gemini's compat layer, Azure, Ollama, and custom providers).
pub struct OpenAiCompat {
    base_url: String,
    api_key: String,
    model: String,
    temperature: Option<f32>,
    max_tokens: u32,
    style: ApiStyle,
    client: reqwest::Client,
}

impl OpenAiCompat {
    /// `base_url` is the API root (e.g. `https://api.openai.com/v1`); the
    /// `/chat/completions` path is appended.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        OpenAiCompat {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            temperature: Some(0.0),
            max_tokens: 8192,
            style: ApiStyle::OpenAi,
            client: reqwest::Client::new(),
        }
    }

    pub fn with_temperature(mut self, t: Option<f32>) -> Self {
        self.temperature = t;
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Switch to Azure OpenAI Service addressing (deployment-path URL + `api-key`
    /// header). `api_version` is the Azure REST API version (e.g.
    /// `2024-12-01-preview`); the `model` passed to [`new`](Self::new) is the
    /// deployment name.
    pub fn with_azure(mut self, api_version: impl Into<String>) -> Self {
        self.style = ApiStyle::Azure {
            api_version: api_version.into(),
        };
        self
    }

    /// The chat-completions URL for this backend's [`ApiStyle`].
    fn endpoint_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match &self.style {
            ApiStyle::OpenAi => format!("{base}/chat/completions"),
            ApiStyle::Azure { api_version } => format!(
                "{base}/openai/deployments/{}/chat/completions?api-version={api_version}",
                self.model
            ),
        }
    }

    /// Post one chat-completion request. `user_content` is the user message's
    /// `content` value — a plain string for text, or a structured array for
    /// multimodal (vision) requests.
    async fn send(&self, system: &str, user_content: Value) -> Result<Completion, LlmError> {
        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user_content},
            ],
            "max_tokens": self.max_tokens,
        });
        // Reasoning models (o1/o3/o4/gpt-5) reject an explicit temperature; omit it.
        if let Some(t) = self.temperature {
            if !model_requires_default_temperature(&self.model) {
                body["temperature"] = json!(t);
            }
        }

        let req = self.client.post(self.endpoint_url()).json(&body);
        // Azure authenticates with an `api-key` header; everyone else uses bearer.
        let req = match &self.style {
            ApiStyle::OpenAi => req.bearer_auth(&self.api_key),
            ApiStyle::Azure { .. } => req.header("api-key", &self.api_key),
        };
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
        let choice = v
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| LlmError::BadResponse("response had no choices".into()))?;
        let content = choice
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop")
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
            input_tokens: tok("prompt_tokens"),
            output_tokens: tok("completion_tokens"),
        })
    }

    /// Like [`send`](Self::send) but with `stream: true` (+ `include_usage`),
    /// consuming the Server-Sent-Events response and accumulating the deltas into
    /// one [`Completion`].
    async fn send_streaming(
        &self,
        system: &str,
        user_content: Value,
    ) -> Result<Completion, LlmError> {
        use futures_util::StreamExt;

        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user_content},
            ],
            "max_tokens": self.max_tokens,
            "stream": true,
            // Ask the final chunk to carry usage so cost accounting still works.
            "stream_options": {"include_usage": true},
        });
        if let Some(t) = self.temperature {
            if !model_requires_default_temperature(&self.model) {
                body["temperature"] = json!(t);
            }
        }

        let req = self.client.post(self.endpoint_url()).json(&body);
        let req = match &self.style {
            ApiStyle::OpenAi => req.bearer_auth(&self.api_key),
            ApiStyle::Azure { .. } => req.header("api-key", &self.api_key),
        };
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body,
            });
        }

        let mut acc = SseAccumulator::default();
        // Accumulate raw bytes and decode only *complete* lines, so a multi-byte
        // UTF-8 sequence split across two network chunks is never lossily decoded
        // mid-sequence.
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let text = String::from_utf8_lossy(&line);
                acc.feed_line(text.trim_end_matches(['\r', '\n']));
            }
        }
        // Flush any final line not terminated by a newline.
        if !buf.is_empty() {
            let text = String::from_utf8_lossy(&buf);
            acc.feed_line(text.trim_end_matches(['\r', '\n']));
        }
        Ok(acc.into_completion())
    }
}

/// Accumulates an OpenAI Chat-Completions SSE stream into a [`Completion`].
#[derive(Default)]
struct SseAccumulator {
    content: String,
    finish_reason: String,
    input_tokens: u32,
    output_tokens: u32,
}

impl SseAccumulator {
    /// Feed one SSE line (`data: {...}`); non-data lines and `[DONE]` are ignored.
    fn feed_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
            if let Some(piece) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
            {
                self.content.push_str(piece);
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = fr.to_string();
            }
        }
        if let Some(usage) = v.get("usage") {
            let tok = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            self.input_tokens = tok("prompt_tokens");
            self.output_tokens = tok("completion_tokens");
        }
    }

    fn into_completion(self) -> Completion {
        Completion {
            content: self.content,
            finish_reason: if self.finish_reason.is_empty() {
                "stop".into()
            } else {
                self.finish_reason
            },
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        }
    }
}

/// True if `model` is an OpenAI reasoning model (o1/o3/o4 family or gpt-5) that
/// rejects an explicit `temperature` and returns HTTP 400 if any value — including
/// 0 — is sent; the parameter must be omitted entirely.
pub fn model_requires_default_temperature(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // Strip a leading "openai/" or gateway prefix some providers prepend.
    let base = m.rsplit('/').next().unwrap_or(&m);
    if base.starts_with("gpt-5") {
        return true;
    }
    ["o1", "o3", "o4"]
        .iter()
        .any(|fam| base == *fam || base.starts_with(&format!("{fam}-")))
}

#[async_trait]
impl LlmClient for OpenAiCompat {
    async fn complete(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        self.send(system, Value::String(user.to_string())).await
    }

    async fn complete_with_content(
        &self,
        system: &str,
        user: &Value,
    ) -> Result<Completion, LlmError> {
        // Transmit the structured (possibly multimodal) content verbatim so image
        // parts actually reach the model.
        self.send(system, user.clone()).await
    }

    async fn complete_streaming(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        self.send_streaming(system, Value::String(user.to_string()))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn parses_a_successful_completion() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"content": "{\"nodes\": []}"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 3}
            })))
            .mount(&server)
            .await;

        let client = OpenAiCompat::new(server.uri(), "test-key", "gpt-test");
        let c = client
            .complete("system prompt", "user content")
            .await
            .unwrap();
        assert_eq!(c.content, "{\"nodes\": []}");
        assert_eq!(c.finish_reason, "stop");
        assert_eq!(c.input_tokens, 12);
        assert_eq!(c.output_tokens, 3);
        assert!(!c.is_truncated());
    }

    /// Pull the JSON body of the first request the mock received.
    async fn first_body(server: &MockServer) -> Value {
        let reqs = server.received_requests().await.unwrap();
        serde_json::from_slice(&reqs[0].body).unwrap()
    }

    #[tokio::test]
    async fn omits_temperature_for_reasoning_models() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{}"}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;
        // o-series / gpt-5 reasoning models reject an explicit temperature (HTTP 400).
        let client = OpenAiCompat::new(server.uri(), "k", "o3-mini");
        client.complete("s", "u").await.unwrap();
        let body = first_body(&server).await;
        assert!(
            body.get("temperature").is_none(),
            "reasoning model must omit temperature, got {body}"
        );
    }

    #[tokio::test]
    async fn sends_temperature_for_standard_models() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{}"}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;
        let client = OpenAiCompat::new(server.uri(), "k", "gpt-4o-mini");
        client.complete("s", "u").await.unwrap();
        let body = first_body(&server).await;
        assert_eq!(
            body.get("temperature"),
            Some(&json!(0.0)),
            "standard model keeps the configured temperature"
        );
    }

    #[test]
    fn reasoning_model_detection() {
        for m in [
            "o1",
            "o3-mini",
            "o4-mini",
            "gpt-5",
            "gpt-5-mini",
            "openai/o3-mini",
        ] {
            assert!(
                model_requires_default_temperature(m),
                "{m} is a reasoning model"
            );
        }
        for m in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1-mini",
            "deepseek-chat",
            "o-other",
        ] {
            assert!(
                !model_requires_default_temperature(m),
                "{m} is not a reasoning model"
            );
        }
    }

    #[tokio::test]
    async fn truncated_reply_is_flagged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"nodes\": [partial"}, "finish_reason": "length"}]
            })))
            .mount(&server)
            .await;
        let client = OpenAiCompat::new(server.uri(), "k", "m");
        let c = client.complete("s", "u").await.unwrap();
        assert!(c.is_truncated());
    }

    #[tokio::test]
    async fn complete_with_content_transmits_structured_multimodal_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{}"}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;
        let client = OpenAiCompat::new(server.uri(), "k", "gpt-4o");
        // The array shape `vision::openai_content` produces for an image.
        let content = json!([
            {"type": "text", "text": "describe"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk=", "detail": "auto"}}
        ]);
        client.complete_with_content("s", &content).await.unwrap();
        let body = first_body(&server).await;
        let user_content = &body["messages"][1]["content"];
        assert!(
            user_content.is_array(),
            "multimodal content must be sent as an array, got {user_content}"
        );
        assert_eq!(user_content[1]["type"], json!("image_url"));
        assert_eq!(
            user_content[1]["image_url"]["url"],
            json!("data:image/png;base64,aGk=")
        );
    }

    #[tokio::test]
    async fn azure_uses_deployment_path_and_api_key_header() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        // Azure routes by deployment path + api-version query + `api-key` header
        // (NOT bearer auth on a bare /chat/completions path).
        Mock::given(method("POST"))
            .and(path("/openai/deployments/my-deploy/chat/completions"))
            .and(query_param("api-version", "2024-12-01-preview"))
            .and(header("api-key", "az-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"nodes\": []}"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2}
            })))
            .mount(&server)
            .await;

        let client =
            OpenAiCompat::new(server.uri(), "az-key", "my-deploy").with_azure("2024-12-01-preview");
        let c = client.complete("system", "user").await.unwrap();
        assert_eq!(c.content, "{\"nodes\": []}");
        assert_eq!(c.input_tokens, 5);
    }

    #[tokio::test]
    async fn streaming_accumulates_sse_deltas_into_a_completion() {
        let server = MockServer::start().await;
        // A canonical OpenAI SSE stream: two content deltas, a finish chunk, then
        // a usage-only chunk (include_usage), then [DONE].
        let sse =
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
            data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n\
            data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;
        let client = OpenAiCompat::new(server.uri(), "k", "gpt-4o-mini");
        let c = client.complete_streaming("s", "u").await.unwrap();
        assert_eq!(c.content, "Hello world");
        assert_eq!(c.finish_reason, "stop");
        assert_eq!(c.input_tokens, 7);
        assert_eq!(c.output_tokens, 2);
        // The request must have asked for a stream.
        let body = first_body(&server).await;
        assert_eq!(body.get("stream"), Some(&json!(true)));
    }

    #[tokio::test]
    async fn error_status_surfaces_body_for_overflow_detection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("This model's maximum context length is 8192 tokens"),
            )
            .mount(&server)
            .await;
        let client = OpenAiCompat::new(server.uri(), "k", "m");
        let err = client.complete("s", "u").await.unwrap_err();
        assert!(
            err.is_context_overflow(),
            "should detect context overflow: {err}"
        );
    }
}
