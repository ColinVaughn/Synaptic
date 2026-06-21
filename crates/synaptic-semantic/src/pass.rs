//! The semantic pass: documents → LLM → concept nodes/edges.

use synaptic_core::{Edge, Node};
use synaptic_llm::{
    extract_corpus, Document, Fragment, LlmClient, LlmError, SemanticCache, EXTRACTION_SYSTEM,
};

use crate::convert::fragment_to_graph;

/// Default per-chunk token budget for the semantic pass.
pub const TOKEN_BUDGET: usize = 60_000;
/// Default recursive-bisect retry depth.
pub const MAX_RETRY_DEPTH: usize = 3;

/// Result of a semantic pass: the extracted graph plus the token usage that
/// produced it (for cost estimation). A cache hit reports `0` tokens — no API
/// call was made, so it incurred no cost.
#[derive(Debug, Clone, Default)]
pub struct SemanticOutcome {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Run the semantic extraction over `docs` (`(relative_path, content)` pairs) and
/// return the resulting concept nodes/edges plus token usage. When `cache` is
/// set, the whole-corpus result is cached (a coarse cache; per-file incremental
/// would be a future refinement).
pub async fn run_semantic_pass(
    client: &dyn LlmClient,
    docs: Vec<(String, String)>,
    cache: Option<&SemanticCache>,
    concurrency: usize,
) -> Result<SemanticOutcome, LlmError> {
    if docs.is_empty() {
        return Ok(SemanticOutcome::default());
    }
    let key = corpus_key(&docs);
    if let Some(cache) = cache {
        if let Some(v) = cache.get("__corpus__", &key) {
            // Cache hit: no API call, so report no token cost.
            let (nodes, edges) = fragment_to_graph(&Fragment::from_value(&v));
            return Ok(SemanticOutcome {
                nodes,
                edges,
                input_tokens: 0,
                output_tokens: 0,
            });
        }
    }
    let documents: Vec<Document> = docs
        .iter()
        .map(|(rel, content)| Document {
            rel: rel.clone(),
            content: content.clone(),
        })
        .collect();
    let frag = extract_corpus(
        client,
        EXTRACTION_SYSTEM,
        documents,
        TOKEN_BUDGET,
        MAX_RETRY_DEPTH,
        concurrency,
    )
    .await?;
    if let Some(cache) = cache {
        let _ = cache.put("__corpus__", &key, &frag.to_value());
    }
    let (input_tokens, output_tokens) = (frag.input_tokens, frag.output_tokens);
    let (nodes, edges) = fragment_to_graph(&frag);
    Ok(SemanticOutcome {
        nodes,
        edges,
        input_tokens,
        output_tokens,
    })
}

/// Deterministic cache key over the (sorted) corpus.
fn corpus_key(docs: &[(String, String)]) -> String {
    let mut parts: Vec<String> = docs.iter().map(|(r, c)| format!("{r}\u{0}{c}")).collect();
    parts.sort();
    parts.join("\u{0}\u{0}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use synaptic_llm::Completion;

    /// Mock backend: returns one concept node per `<untrusted_source>` block,
    /// keyed by the doc's path. Counts calls.
    struct Mock {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmClient for Mock {
        async fn complete(&self, _system: &str, user: &str) -> Result<Completion, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let nodes: Vec<String> = user
                .split("path=\"")
                .skip(1)
                .filter_map(|s| s.split('"').next())
                .map(|p| format!("{{\"id\":\"{p}_c\",\"label\":\"Concept of {p}\",\"file_type\":\"concept\",\"source_file\":\"{p}\"}}"))
                .collect();
            Ok(Completion {
                content: format!("{{\"nodes\":[{}],\"edges\":[]}}", nodes.join(",")),
                finish_reason: "stop".into(),
                input_tokens: 1,
                output_tokens: 1,
            })
        }
    }

    fn docs() -> Vec<(String, String)> {
        vec![
            ("a.md".into(), "auth notes".into()),
            ("b.md".into(), "session notes".into()),
        ]
    }

    #[tokio::test]
    async fn extracts_concept_nodes_from_docs() {
        let client = Mock {
            calls: AtomicUsize::new(0),
        };
        let out = run_semantic_pass(&client, docs(), None, 4).await.unwrap();
        let nodes = out.nodes;
        assert_eq!(nodes.len(), 2);
        let labels: Vec<&str> = nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"Concept of a.md"));
        assert!(nodes
            .iter()
            .all(|n| n.file_type == synaptic_core::FileType::Concept));
    }

    #[tokio::test]
    async fn surfaces_token_usage_for_cost_estimation() {
        let client = Mock {
            calls: AtomicUsize::new(0),
        };
        let out = run_semantic_pass(&client, docs(), None, 4).await.unwrap();
        assert!(
            out.input_tokens > 0 && out.output_tokens > 0,
            "a fresh pass must report the tokens it spent"
        );
    }

    #[tokio::test]
    async fn cache_hit_reports_zero_cost() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SemanticCache::new(dir.path());
        let client = Mock {
            calls: AtomicUsize::new(0),
        };
        let _ = run_semantic_pass(&client, docs(), Some(&cache), 4)
            .await
            .unwrap();
        let out = run_semantic_pass(&client, docs(), Some(&cache), 4)
            .await
            .unwrap();
        assert_eq!(out.input_tokens, 0);
        assert_eq!(out.output_tokens, 0);
    }

    #[tokio::test]
    async fn cache_hit_skips_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SemanticCache::new(dir.path());
        let client = Mock {
            calls: AtomicUsize::new(0),
        };
        let _ = run_semantic_pass(&client, docs(), Some(&cache), 4)
            .await
            .unwrap();
        let after_first = client.calls.load(Ordering::SeqCst);
        assert!(after_first >= 1);
        // Second run with the same docs hits the corpus cache, so no new calls.
        let out = run_semantic_pass(&client, docs(), Some(&cache), 4)
            .await
            .unwrap();
        assert_eq!(
            client.calls.load(Ordering::SeqCst),
            after_first,
            "cache hit must not call the model"
        );
        assert_eq!(out.nodes.len(), 2);
    }
}
