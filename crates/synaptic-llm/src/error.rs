use thiserror::Error;

/// Errors from the LLM layer.
#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("malformed provider response: {0}")]
    BadResponse(String),
    #[error("no backend configured (set an API key env var, e.g. OPENAI_API_KEY)")]
    NoBackend,
    #[error("backend configuration error: {0}")]
    Config(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl LlmError {
    /// True if this error looks like a context-length / token-overflow failure
    /// — the signal to bisect the input and retry.
    pub fn is_context_overflow(&self) -> bool {
        let msg = match self {
            LlmError::Status { body, .. } => body.to_lowercase(),
            LlmError::BadResponse(m) => m.to_lowercase(),
            _ => return false,
        };
        const MARKERS: &[&str] = &[
            "context size",
            "context length",
            "context_length",
            "context window",
            "n_keep",
            "exceeds the available",
            "n_ctx",
            "maximum context",
            "too many tokens",
            "prompt is too long",
            "context_length_exceeded",
        ];
        MARKERS.iter().any(|m| msg.contains(m))
    }
}
