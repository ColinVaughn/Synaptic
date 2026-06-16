//! CodeGraph LLM provider layer: the semantic-pass infrastructure.
//!
//! Scope (one backend + cache + retry):
//! - [`LlmClient`] trait + an OpenAI-compatible backend ([`OpenAiCompat`]),
//! - a [`registry`] of OpenAI-compatible providers with env-var auto-detection,
//! - a content-hash [`SemanticCache`],
//! - robust [`parse_llm_json`] response repair,
//! - prompt-injection wrapping ([`wrap_untrusted`]),
//! - token-budget [`chunk_by_tokens`] + [`extract_with_adaptive_retry`]
//!   (recursive bisect on context overflow).
//!
//! All network behavior is exercised with `wiremock` — no real keys or calls.
#![forbid(unsafe_code)]

pub mod anthropic;
pub mod bedrock;
pub mod cache;
pub mod claude_cli;
pub mod cost;
pub mod error;
pub mod extract;
pub mod prompts;
pub mod provider;
pub mod registry;
pub mod sigv4;
pub mod text;
pub mod vision;

pub use anthropic::{Anthropic, ANTHROPIC_DEFAULT_MODEL};
pub use bedrock::{Bedrock, BEDROCK_DEFAULT_MODEL};
pub use cache::SemanticCache;
pub use claude_cli::ClaudeCli;
pub use cost::{estimate_cost, pricing, Pricing};
pub use error::LlmError;
pub use extract::{extract_corpus, extract_with_adaptive_retry, Document, Fragment};
pub use prompts::EXTRACTION_SYSTEM;
pub use provider::{Completion, LlmClient, OpenAiCompat};
pub use registry::{
    build_client, default_concurrency, detect_backend, detect_backend_env, make_backend,
    resolve_backend, BackendConfig, ALL_BACKENDS, BACKENDS,
};
pub use sigv4::Credentials;
pub use text::{chunk_by_tokens, count_tokens, estimate_tokens, parse_llm_json, wrap_untrusted};
pub use vision::{
    anthropic_content, backend_supports_vision, build_image_refs, image_notes, is_vision_image,
    openai_content, partition_semantic_files, strip_pixels, with_image_notes, ImageRef,
    IMAGE_TOKEN_ESTIMATE, MAX_IMAGES_PER_CHUNK,
};
