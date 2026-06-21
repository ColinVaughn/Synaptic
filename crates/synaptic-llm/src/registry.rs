//! Registry of LLM providers + env-var auto-detection. The OpenAI-compatible
//! slice is [`BACKENDS`] + [`detect_backend`]/[`make_backend`];
//! [`resolve_backend`]/[`build_client`] add the native Anthropic, Bedrock, and
//! claude-CLI backends and return a boxed [`LlmClient`] following the full
//! provider priority.

use crate::anthropic::{Anthropic, ANTHROPIC_DEFAULT_MODEL};
use crate::bedrock::{Bedrock, BEDROCK_DEFAULT_MODEL};
use crate::claude_cli::ClaudeCli;
use crate::error::LlmError;
use crate::provider::{LlmClient, OpenAiCompat};
use crate::sigv4::Credentials;

/// Static description of one OpenAI-compatible provider.
#[derive(Debug, Clone, Copy)]
pub struct BackendConfig {
    pub name: &'static str,
    pub base_url: &'static str,
    pub default_model: &'static str,
    /// API-key env vars; the first one set is used.
    pub env_keys: &'static [&'static str],
    /// Optional env var overriding the model.
    pub model_env: Option<&'static str>,
    /// Optional env var overriding the base URL (Ollama, Azure).
    pub base_url_env: Option<&'static str>,
    /// Whether the backend's default model can see images (vision). Ollama is
    /// `false` here but opt-in at runtime (see `vision::backend_supports_vision`).
    pub vision: bool,
}

/// Known OpenAI-compatible backends, in detection-priority order
/// (`detect_backend`: gemini → kimi → openai → deepseek → azure → ollama).
pub static BACKENDS: &[BackendConfig] = &[
    BackendConfig {
        name: "gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        default_model: "gemini-2.5-flash",
        env_keys: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        model_env: Some("GEMINI_MODEL"),
        base_url_env: None,
        vision: true,
    },
    BackendConfig {
        name: "kimi",
        base_url: "https://api.moonshot.ai/v1",
        default_model: "kimi-k2",
        env_keys: &["MOONSHOT_API_KEY"],
        model_env: Some("MOONSHOT_MODEL"),
        base_url_env: None,
        vision: true,
    },
    BackendConfig {
        name: "openai",
        base_url: "https://api.openai.com/v1",
        default_model: "gpt-4.1-mini",
        env_keys: &["OPENAI_API_KEY"],
        model_env: Some("OPENAI_MODEL"),
        base_url_env: Some("OPENAI_BASE_URL"),
        vision: true,
    },
    BackendConfig {
        name: "deepseek",
        base_url: "https://api.deepseek.com",
        default_model: "deepseek-chat",
        env_keys: &["DEEPSEEK_API_KEY"],
        model_env: Some("DEEPSEEK_MODEL"),
        base_url_env: None,
        vision: false,
    },
    BackendConfig {
        name: "azure",
        base_url: "",
        default_model: "gpt-4o",
        env_keys: &["AZURE_OPENAI_API_KEY"],
        model_env: Some("AZURE_OPENAI_DEPLOYMENT"),
        base_url_env: Some("AZURE_OPENAI_ENDPOINT"),
        vision: true,
    },
    BackendConfig {
        name: "ollama",
        base_url: "http://localhost:11434/v1",
        default_model: "qwen2.5-coder:7b",
        env_keys: &["OLLAMA_API_KEY"],
        model_env: Some("OLLAMA_MODEL"),
        base_url_env: Some("OLLAMA_BASE_URL"),
        vision: false,
    },
];

fn lookup(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key).filter(|v| !v.is_empty())
}

fn is_available(cfg: &BackendConfig, get: &impl Fn(&str) -> Option<String>) -> bool {
    let has_key = cfg.env_keys.iter().any(|k| lookup(get, k).is_some());
    match cfg.name {
        // Azure needs both a key and an endpoint.
        "azure" => has_key && cfg.base_url_env.and_then(|e| lookup(get, e)).is_some(),
        // Ollama is local and keyless; presence of its base URL opts in.
        "ollama" => cfg.base_url_env.and_then(|e| lookup(get, e)).is_some(),
        _ => has_key,
    }
}

/// Choose a backend from the environment via the provided lookup, honoring the
/// priority order. Use [`detect_backend_env`] for the real process environment.
pub fn detect_backend(get: &impl Fn(&str) -> Option<String>) -> Option<&'static BackendConfig> {
    BACKENDS.iter().find(|cfg| is_available(cfg, get))
}

/// [`detect_backend`] reading the real process environment.
pub fn detect_backend_env() -> Option<&'static BackendConfig> {
    detect_backend(&|k| std::env::var(k).ok())
}

/// Default Azure REST API version when `AZURE_OPENAI_API_VERSION` is unset.
const AZURE_DEFAULT_API_VERSION: &str = "2024-12-01-preview";

/// Resolve the `SYNAPTIC_LLM_TEMPERATURE` override. Returns:
/// - `None` — no override set (leave the backend default; reasoning models are
///   still handled per-request in [`OpenAiCompat`]).
/// - `Some(None)` — `none`/`omit`/`default`: omit the parameter entirely.
/// - `Some(Some(v))` — a numeric value to send verbatim. A non-numeric value is
///   ignored (treated as no override).
fn temperature_override(get: &impl Fn(&str) -> Option<String>) -> Option<Option<f32>> {
    let raw = lookup(get, "SYNAPTIC_LLM_TEMPERATURE")?;
    let raw = raw.trim();
    match raw.to_ascii_lowercase().as_str() {
        "none" | "omit" | "default" => Some(None),
        _ => raw.parse::<f32>().ok().map(Some),
    }
}

/// Construct a client for `cfg` from the environment (`get`), applying model and
/// base-URL overrides and the first available API key (empty for keyless Ollama).
pub fn make_backend(
    cfg: &BackendConfig,
    get: &impl Fn(&str) -> Option<String>,
) -> Result<OpenAiCompat, LlmError> {
    if !is_available(cfg, get) {
        return Err(LlmError::NoBackend);
    }
    let base_url = cfg
        .base_url_env
        .and_then(|e| lookup(get, e))
        .unwrap_or_else(|| cfg.base_url.to_string());
    let model = cfg
        .model_env
        .and_then(|e| lookup(get, e))
        .unwrap_or_else(|| cfg.default_model.to_string());
    let api_key = cfg
        .env_keys
        .iter()
        .find_map(|k| lookup(get, k))
        .unwrap_or_default();
    let mut client = OpenAiCompat::new(base_url, api_key, model);
    // Azure addresses the model as a deployment-path URL + `api-key` header.
    if cfg.name == "azure" {
        let api_version = lookup(get, "AZURE_OPENAI_API_VERSION")
            .unwrap_or_else(|| AZURE_DEFAULT_API_VERSION.to_string());
        client = client.with_azure(api_version);
    }
    if let Some(t) = temperature_override(get) {
        client = client.with_temperature(t);
    }
    Ok(client)
}

/// Every backend [`resolve_backend`]/[`build_client`] understands, in priority
/// order. `claude-cli` is opt-in (never auto-detected) — select it with
/// `SYNAPTIC_BACKEND=claude-cli`.
pub static ALL_BACKENDS: &[&str] = &[
    "gemini",
    "kimi",
    "claude",
    "openai",
    "deepseek",
    "azure",
    "bedrock",
    "ollama",
    "claude-cli",
];

fn has(get: &impl Fn(&str) -> Option<String>, key: &str) -> bool {
    lookup(get, key).is_some()
}

/// Pick a backend from the environment, honoring the full provider priority
/// (gemini → kimi → claude → openai → deepseek → azure → bedrock → ollama).
/// `SYNAPTIC_BACKEND` forces a specific backend (the only way to select the
/// opt-in `claude-cli`). Returns the backend name, or `None` if none is
/// configured. Bedrock is detected from `AWS_ACCESS_KEY_ID` (env credentials
/// only — profile/instance-role resolution is out of scope).
pub fn resolve_backend(get: &impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    if let Some(forced) = lookup(get, "SYNAPTIC_BACKEND") {
        return ALL_BACKENDS.iter().copied().find(|n| *n == forced);
    }
    if has(get, "GEMINI_API_KEY") || has(get, "GOOGLE_API_KEY") {
        return Some("gemini");
    }
    if has(get, "MOONSHOT_API_KEY") {
        return Some("kimi");
    }
    if has(get, "ANTHROPIC_API_KEY") {
        return Some("claude");
    }
    if has(get, "OPENAI_API_KEY") {
        return Some("openai");
    }
    if has(get, "DEEPSEEK_API_KEY") {
        return Some("deepseek");
    }
    if has(get, "AZURE_OPENAI_API_KEY") && has(get, "AZURE_OPENAI_ENDPOINT") {
        return Some("azure");
    }
    // Env credentials only; profile/instance-role resolution is out of scope.
    if has(get, "AWS_ACCESS_KEY_ID") {
        return Some("bedrock");
    }
    if has(get, "OLLAMA_BASE_URL") {
        return Some("ollama");
    }
    None
}

/// How many extraction chunks a backend may run concurrently. A local single-GPU
/// server (`ollama`) and the stateful `claude-cli` subprocess must stay serial;
/// every networked API tolerates a small fan-out (4).
pub fn default_concurrency(backend: &str) -> usize {
    match backend {
        "ollama" | "claude-cli" => 1,
        _ => 4,
    }
}

/// Build a boxed client for `name` from the environment. The OpenAI-compatible
/// names delegate to [`make_backend`]; `claude`/`bedrock`/`claude-cli` build
/// their native clients.
pub fn build_client(
    name: &str,
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Box<dyn LlmClient>, LlmError> {
    match name {
        "claude" => {
            let key = lookup(get, "ANTHROPIC_API_KEY")
                .ok_or_else(|| LlmError::Config("ANTHROPIC_API_KEY not set".into()))?;
            let base = lookup(get, "ANTHROPIC_BASE_URL")
                .unwrap_or_else(|| "https://api.anthropic.com".into());
            let model =
                lookup(get, "ANTHROPIC_MODEL").unwrap_or_else(|| ANTHROPIC_DEFAULT_MODEL.into());
            Ok(Box::new(Anthropic::new(base, key, model)))
        }
        "bedrock" => {
            let access_key = lookup(get, "AWS_ACCESS_KEY_ID")
                .ok_or_else(|| LlmError::Config("AWS_ACCESS_KEY_ID not set".into()))?;
            let secret_key = lookup(get, "AWS_SECRET_ACCESS_KEY").ok_or_else(|| {
                LlmError::Config(
                    "AWS_SECRET_ACCESS_KEY not set (Bedrock needs env credentials; \
                    profile/instance-role resolution is not supported)"
                        .into(),
                )
            })?;
            let region = lookup(get, "AWS_REGION")
                .or_else(|| lookup(get, "AWS_DEFAULT_REGION"))
                .unwrap_or_else(|| "us-east-1".into());
            let model =
                lookup(get, "BEDROCK_MODEL").unwrap_or_else(|| BEDROCK_DEFAULT_MODEL.into());
            let creds = Credentials {
                access_key,
                secret_key,
                session_token: lookup(get, "AWS_SESSION_TOKEN"),
            };
            Ok(Box::new(Bedrock::new(region, creds, model)))
        }
        "claude-cli" => {
            let cli = match lookup(get, "CLAUDE_CLI_MODEL") {
                Some(m) => ClaudeCli::new().with_model(m),
                None => ClaudeCli::new(),
            };
            Ok(Box::new(cli))
        }
        // OpenAI-compatible backends route through the existing registry.
        _ => {
            let cfg = BACKENDS
                .iter()
                .find(|c| c.name == name)
                .ok_or(LlmError::NoBackend)?;
            Ok(Box::new(make_backend(cfg, get)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn no_env_means_no_backend() {
        assert!(detect_backend(&env(&[])).is_none());
    }

    #[test]
    fn detects_openai_from_key() {
        let cfg = detect_backend(&env(&[("OPENAI_API_KEY", "sk-x")])).unwrap();
        assert_eq!(cfg.name, "openai");
    }

    #[test]
    fn priority_prefers_gemini_over_openai() {
        let cfg =
            detect_backend(&env(&[("OPENAI_API_KEY", "x"), ("GEMINI_API_KEY", "y")])).unwrap();
        assert_eq!(cfg.name, "gemini");
    }

    #[test]
    fn azure_requires_endpoint_not_just_key() {
        assert!(detect_backend(&env(&[("AZURE_OPENAI_API_KEY", "k")])).is_none());
        let cfg = detect_backend(&env(&[
            ("AZURE_OPENAI_API_KEY", "k"),
            ("AZURE_OPENAI_ENDPOINT", "https://x.openai.azure.com"),
        ]))
        .unwrap();
        assert_eq!(cfg.name, "azure");
    }

    #[test]
    fn ollama_detected_by_base_url_without_key() {
        let cfg =
            detect_backend(&env(&[("OLLAMA_BASE_URL", "http://localhost:11434/v1")])).unwrap();
        assert_eq!(cfg.name, "ollama");
    }

    #[test]
    fn make_backend_applies_overrides() {
        let get = env(&[("OPENAI_API_KEY", "sk-x"), ("OPENAI_MODEL", "gpt-custom")]);
        let cfg = detect_backend(&get).unwrap();
        // Construction succeeds (model override applied internally).
        assert!(make_backend(cfg, &get).is_ok());
    }

    mod unified {
        use super::*;

        #[test]
        fn resolves_claude_from_anthropic_key() {
            assert_eq!(
                resolve_backend(&env(&[("ANTHROPIC_API_KEY", "sk-ant")])),
                Some("claude")
            );
        }

        #[test]
        fn resolves_bedrock_from_aws_access_key() {
            assert_eq!(
                resolve_backend(&env(&[("AWS_ACCESS_KEY_ID", "AKIA")])),
                Some("bedrock")
            );
        }

        #[test]
        fn priority_prefers_gemini_then_claude_over_openai() {
            // gemini wins over everything.
            assert_eq!(
                resolve_backend(&env(&[
                    ("OPENAI_API_KEY", "x"),
                    ("ANTHROPIC_API_KEY", "y"),
                    ("GEMINI_API_KEY", "z"),
                ])),
                Some("gemini")
            );
            // claude outranks openai in the provider priority order.
            assert_eq!(
                resolve_backend(&env(&[("OPENAI_API_KEY", "x"), ("ANTHROPIC_API_KEY", "y")])),
                Some("claude")
            );
        }

        #[test]
        fn explicit_override_selects_opt_in_claude_cli() {
            // claude-cli is never auto-detected; the override is the only way in.
            assert_eq!(
                resolve_backend(&env(&[("SYNAPTIC_BACKEND", "claude-cli")])),
                Some("claude-cli")
            );
            // An unknown override name resolves to nothing.
            assert_eq!(
                resolve_backend(&env(&[("SYNAPTIC_BACKEND", "bogus")])),
                None
            );
        }

        #[test]
        fn no_env_resolves_to_nothing() {
            assert_eq!(resolve_backend(&env(&[])), None);
        }

        #[test]
        fn builds_a_native_anthropic_client() {
            let get = env(&[("ANTHROPIC_API_KEY", "sk-ant")]);
            assert!(build_client("claude", &get).is_ok());
        }

        #[test]
        fn builds_a_bedrock_client_with_env_credentials() {
            let get = env(&[
                ("AWS_ACCESS_KEY_ID", "AKIA"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
                ("AWS_REGION", "us-west-2"),
            ]);
            assert!(build_client("bedrock", &get).is_ok());
        }

        #[test]
        fn bedrock_without_secret_is_a_config_error() {
            let get = env(&[("AWS_ACCESS_KEY_ID", "AKIA")]);
            assert!(matches!(
                build_client("bedrock", &get),
                Err(LlmError::Config(_))
            ));
        }

        #[test]
        fn builds_the_opt_in_claude_cli_client() {
            assert!(build_client("claude-cli", &env(&[])).is_ok());
        }

        #[test]
        fn builds_an_openai_compatible_client_via_make_backend() {
            let get = env(&[("OPENAI_API_KEY", "sk-x")]);
            assert!(build_client("openai", &get).is_ok());
        }
    }

    mod wiring {
        use super::*;
        use crate::provider::LlmClient;
        use serde_json::{json, Value};
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

        async fn ok_mock(server: &MockServer, m: MockBuilder) {
            m.respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{}"}, "finish_reason": "stop"}]
            })))
            .mount(server)
            .await;
        }

        async fn first_body(server: &MockServer) -> Value {
            let reqs = server.received_requests().await.unwrap();
            serde_json::from_slice(&reqs[0].body).unwrap()
        }

        #[tokio::test]
        async fn make_backend_builds_a_working_azure_client() {
            let server = MockServer::start().await;
            ok_mock(
                &server,
                Mock::given(method("POST"))
                    .and(path("/openai/deployments/my-deploy/chat/completions"))
                    .and(query_param("api-version", "2024-12-01-preview"))
                    .and(header("api-key", "az-key")),
            )
            .await;
            let get = env(&[
                ("AZURE_OPENAI_API_KEY", "az-key"),
                ("AZURE_OPENAI_ENDPOINT", &server.uri()),
                ("AZURE_OPENAI_DEPLOYMENT", "my-deploy"),
            ]);
            let cfg = detect_backend(&get).unwrap();
            assert_eq!(cfg.name, "azure");
            let client = make_backend(cfg, &get).unwrap();
            // Would 404 (no mock match) if the client weren't Azure-styled.
            client.complete("s", "u").await.unwrap();
        }

        #[tokio::test]
        async fn temperature_env_override_is_applied() {
            let server = MockServer::start().await;
            ok_mock(
                &server,
                Mock::given(method("POST")).and(path("/chat/completions")),
            )
            .await;
            let get = env(&[
                ("OPENAI_API_KEY", "k"),
                ("OPENAI_BASE_URL", &server.uri()),
                ("SYNAPTIC_LLM_TEMPERATURE", "0.7"),
            ]);
            let client = make_backend(detect_backend(&get).unwrap(), &get).unwrap();
            client.complete("s", "u").await.unwrap();
            // f32 0.7 widens to ~0.6999999 as JSON; compare with tolerance.
            let temp = first_body(&server).await["temperature"].as_f64().unwrap();
            assert!(
                (temp - 0.7).abs() < 1e-6,
                "temperature {temp} should be ~0.7"
            );
        }

        #[tokio::test]
        async fn temperature_env_none_omits_the_parameter() {
            let server = MockServer::start().await;
            ok_mock(
                &server,
                Mock::given(method("POST")).and(path("/chat/completions")),
            )
            .await;
            let get = env(&[
                ("OPENAI_API_KEY", "k"),
                ("OPENAI_BASE_URL", &server.uri()),
                ("SYNAPTIC_LLM_TEMPERATURE", "none"),
            ]);
            let client = make_backend(detect_backend(&get).unwrap(), &get).unwrap();
            client.complete("s", "u").await.unwrap();
            assert!(first_body(&server).await.get("temperature").is_none());
        }
    }
}
