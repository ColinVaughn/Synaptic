//! Community labeling: name graph communities via one batched LLM call, falling
//! back to `Community {cid}` placeholders.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use synaptic_llm::{parse_llm_json, LlmClient};

/// Communities per LLM batch (keeps the prompt within a ~16k context window).
pub const LABEL_BATCH_SIZE: usize = 100;
/// Representative member labels shown to the model per community.
pub const LABEL_TOP_K: usize = 8;

const LABEL_SYSTEM: &str = "You are naming clusters in a knowledge graph. For each \
community below, return a concise 2-5 word plain-language name describing what it is \
about (e.g. \"Order Management\", \"Payment Flow\", \"Auth Middleware\"). Respond ONLY \
with a JSON object mapping the community id (as a string) to its name - no prose, no \
markdown fences.";

/// Name each community. `members` maps community id → representative member labels
/// (the caller picks, e.g. highest-degree first). Returns a name for **every**
/// community: any the model does not name (or any batch that fails) keep the
/// `Community {cid}` placeholder. Per-batch failures are logged and skipped.
pub async fn label_communities(
    client: &dyn LlmClient,
    members: &BTreeMap<u32, Vec<String>>,
) -> BTreeMap<u32, String> {
    let mut labels: BTreeMap<u32, String> = members
        .keys()
        .map(|c| (*c, format!("Community {c}")))
        .collect();
    let cids: Vec<u32> = members.keys().copied().collect();
    for batch in cids.chunks(LABEL_BATCH_SIZE) {
        let mut lines = String::new();
        for &cid in batch {
            let sample: Vec<&str> = members[&cid]
                .iter()
                .take(LABEL_TOP_K)
                .map(String::as_str)
                .collect();
            let _ = writeln!(lines, "Community {cid}: {}", sample.join(", "));
        }
        match client.complete(LABEL_SYSTEM, &lines).await {
            Ok(c) => apply_label_response(&parse_llm_json(&c.content), batch, &mut labels),
            Err(e) => eprintln!(
                "[synaptic label] batch of {} communities failed: {e}",
                batch.len()
            ),
        }
    }
    labels
}

/// Merge a parsed `{cid: name}` JSON object into `labels`, accepting only ids in
/// `batch` with non-empty names (capped at 60 chars).
fn apply_label_response(v: &serde_json::Value, batch: &[u32], labels: &mut BTreeMap<u32, String>) {
    let Some(obj) = v.as_object() else {
        return;
    };
    for &cid in batch {
        if let Some(name) = obj.get(&cid.to_string()).and_then(|n| n.as_str()) {
            let name = name.trim();
            if !name.is_empty() {
                labels.insert(cid, name.chars().take(60).collect());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use synaptic_llm::{Completion, LlmError};

    struct Mock(&'static str);
    #[async_trait]
    impl LlmClient for Mock {
        async fn complete(&self, _s: &str, _u: &str) -> Result<Completion, LlmError> {
            Ok(Completion {
                content: self.0.to_string(),
                finish_reason: "stop".into(),
                input_tokens: 1,
                output_tokens: 1,
            })
        }
    }
    struct Dead;
    #[async_trait]
    impl LlmClient for Dead {
        async fn complete(&self, _s: &str, _u: &str) -> Result<Completion, LlmError> {
            Err(LlmError::NoBackend)
        }
    }

    fn members() -> BTreeMap<u32, Vec<String>> {
        [
            (0u32, vec!["login".into(), "auth".into()]),
            (1u32, vec!["order".into()]),
        ]
        .into_iter()
        .collect()
    }

    #[tokio::test]
    async fn names_communities_from_llm_json() {
        let m = Mock(r#"{"0": "Auth Flow", "1": "Order Management"}"#);
        let labels = label_communities(&m, &members()).await;
        assert_eq!(labels[&0], "Auth Flow");
        assert_eq!(labels[&1], "Order Management");
    }

    #[tokio::test]
    async fn falls_back_to_placeholder_on_backend_error() {
        let labels = label_communities(&Dead, &members()).await;
        assert_eq!(labels[&0], "Community 0");
        assert_eq!(labels[&1], "Community 1");
    }

    #[tokio::test]
    async fn unnamed_communities_keep_placeholder() {
        // Model names only community 0.
        let m = Mock(r#"{"0": "Auth Flow"}"#);
        let labels = label_communities(&m, &members()).await;
        assert_eq!(labels[&0], "Auth Flow");
        assert_eq!(labels[&1], "Community 1");
    }

    #[tokio::test]
    async fn tolerates_json_wrapped_in_prose_or_fences() {
        let m = Mock("Here you go:\n```json\n{\"0\":\"Auth\",\"1\":\"Orders\"}\n```");
        let labels = label_communities(&m, &members()).await;
        assert_eq!(labels[&0], "Auth");
        assert_eq!(labels[&1], "Orders");
    }
}
