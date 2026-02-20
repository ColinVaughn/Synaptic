//! Adaptive extraction: chunk documents to a token budget, send each chunk to
//! the model, and on context overflow / truncated output recursively bisect the
//! chunk and merge the partial results.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::error::LlmError;
use crate::provider::LlmClient;
use crate::text::{chunk_by_tokens, estimate_tokens, parse_llm_json, wrap_untrusted};

/// One source document to extract from.
#[derive(Debug, Clone)]
pub struct Document {
    pub rel: String,
    pub content: String,
}

/// Accumulated extraction output (concatenated fragments + token totals).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fragment {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
    pub hyperedges: Vec<Value>,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl Fragment {
    fn from_json(v: &Value) -> Fragment {
        let arr = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        Fragment {
            nodes: arr("nodes"),
            edges: arr("edges"),
            hyperedges: arr("hyperedges"),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    /// Build a fragment from a node-link `Value` (token counts default to 0).
    /// Useful for restoring a cached fragment.
    pub fn from_value(v: &Value) -> Fragment {
        Self::from_json(v)
    }

    /// Serialize the fragment's `nodes`/`edges`/`hyperedges` to a node-link value
    /// (for caching).
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "nodes": self.nodes,
            "edges": self.edges,
            "hyperedges": self.hyperedges,
        })
    }

    fn merge(&mut self, other: Fragment) {
        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
        self.hyperedges.extend(other.hyperedges);
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// Build the user message: each doc wrapped in an `<untrusted_source>` block.
fn build_user(docs: &[Document]) -> String {
    docs.iter()
        .map(|d| wrap_untrusted(&d.rel, &d.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

type Boxed<'a> = Pin<Box<dyn Future<Output = Result<Fragment, LlmError>> + Send + 'a>>;

fn retry<'a>(
    client: &'a dyn LlmClient,
    system: &'a str,
    docs: Vec<Document>,
    max_depth: usize,
    depth: usize,
) -> Boxed<'a> {
    Box::pin(async move {
        let user = build_user(&docs);
        match client.complete(system, &user).await {
            Err(e) if e.is_context_overflow() => {
                // Overflow before any output: bisect, or give up with an empty
                // fragment at the base case.
                if docs.len() <= 1 || depth >= max_depth {
                    return Ok(Fragment::default());
                }
                bisect(client, system, docs, max_depth, depth).await
            }
            Err(e) => Err(e),
            Ok(c) => {
                // Parse up front so we can detect a "hollow" success: HTTP 200,
                // finish_reason "stop", but zero usable nodes/edges/hyperedges,
                // and re-route it through bisection exactly like truncation (a
                // local model under load triggers this). At the base case we keep
                // the empty result.
                let mut frag = Fragment::from_json(&parse_llm_json(&c.content));
                let hollow =
                    frag.nodes.is_empty() && frag.edges.is_empty() && frag.hyperedges.is_empty();
                if (c.is_truncated() || hollow) && docs.len() > 1 && depth < max_depth {
                    bisect(client, system, docs, max_depth, depth).await
                } else {
                    frag.input_tokens = c.input_tokens;
                    frag.output_tokens = c.output_tokens;
                    Ok(frag)
                }
            }
        }
    })
}

async fn bisect<'a>(
    client: &'a dyn LlmClient,
    system: &'a str,
    docs: Vec<Document>,
    max_depth: usize,
    depth: usize,
) -> Result<Fragment, LlmError> {
    let mid = docs.len() / 2;
    let left_docs = docs[..mid].to_vec();
    let right_docs = docs[mid..].to_vec();
    let mut left = retry(client, system, left_docs, max_depth, depth + 1).await?;
    let right = retry(client, system, right_docs, max_depth, depth + 1).await?;
    left.merge(right);
    Ok(left)
}

/// Extract one chunk of documents with adaptive-bisect retry (no chunking).
pub async fn extract_with_adaptive_retry(
    client: &dyn LlmClient,
    system: &str,
    docs: Vec<Document>,
    max_depth: usize,
) -> Result<Fragment, LlmError> {
    if docs.is_empty() {
        return Ok(Fragment::default());
    }
    retry(client, system, docs, max_depth, 0).await
}

/// Extract a whole corpus: pack documents into `token_budget` chunks, run each
/// through [`extract_with_adaptive_retry`] with up to `concurrency` chunks
/// in flight at once, and merge.
///
/// `concurrency` is clamped to ≥ 1; pass 1 for backends that must stay serial
/// (e.g. ollama, claude-cli). `buffered` preserves chunk order, so the merged
/// fragment and per-chunk failure reporting are identical to a sequential run.
pub async fn extract_corpus(
    client: &dyn LlmClient,
    system: &str,
    docs: Vec<Document>,
    token_budget: usize,
    max_depth: usize,
    concurrency: usize,
) -> Result<Fragment, LlmError> {
    use futures_util::stream::{self, StreamExt};

    let sized: Vec<(Document, usize)> = docs
        .into_iter()
        .map(|d| {
            let t = estimate_tokens(&d.content);
            (d, t)
        })
        .collect();
    let chunks = chunk_by_tokens(&sized, token_budget);
    let total = chunks.len();

    // Bounded-concurrent fan-out, order preserved.
    let results: Vec<Result<Fragment, LlmError>> = stream::iter(chunks)
        .map(|chunk| extract_with_adaptive_retry(client, system, chunk, max_depth))
        .buffered(concurrency.max(1))
        .collect()
        .await;

    let mut merged = Fragment::default();
    let mut failed = 0usize;
    for (i, r) in results.into_iter().enumerate() {
        // One bad chunk (a non-overflow API error; overflow is already handled
        // inside the bisect) must not abort the whole corpus: log and continue
        // with partial results.
        match r {
            Ok(frag) => merged.merge(frag),
            Err(e) => {
                eprintln!("[codegraph] semantic chunk {}/{total} failed: {e}", i + 1);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        eprintln!(
            "[codegraph] WARNING: {failed}/{total} semantic chunk(s) failed — partial results returned."
        );
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Completion;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock backend that "overflows" on multi-doc inputs and succeeds on singles,
    /// returning one node whose id is the doc's path. Counts calls.
    struct Bisecting {
        calls: AtomicUsize,
        /// "length" → truncated output; "error" → context-overflow error.
        mode: &'static str,
    }

    #[async_trait]
    impl LlmClient for Bisecting {
        async fn complete(&self, _system: &str, user: &str) -> Result<Completion, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let n = user.matches("<untrusted_source").count();
            if n > 1 {
                return match self.mode {
                    "error" => Err(LlmError::Status {
                        status: 400,
                        body: "context_length_exceeded: too many tokens".into(),
                    }),
                    _ => Ok(Completion {
                        content: "{\"nodes\": [trunc".into(),
                        finish_reason: "length".into(),
                        input_tokens: 0,
                        output_tokens: 0,
                    }),
                };
            }
            // single doc -> succeed with a node keyed by the path attribute.
            let id = user
                .split("path=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("x");
            Ok(Completion {
                content: format!("{{\"nodes\": [{{\"id\": \"{id}\"}}], \"edges\": []}}"),
                finish_reason: "stop".into(),
                input_tokens: 1,
                output_tokens: 1,
            })
        }
    }

    fn docs(n: usize) -> Vec<Document> {
        (0..n)
            .map(|i| Document {
                rel: format!("d{i}.md"),
                content: format!("content {i}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn truncation_bisects_to_singles_and_merges() {
        let client = Bisecting {
            calls: AtomicUsize::new(0),
            mode: "length",
        };
        let frag = extract_with_adaptive_retry(&client, "sys", docs(4), 5)
            .await
            .unwrap();
        assert_eq!(frag.nodes.len(), 4, "all four docs extracted via bisect");
        let ids: Vec<&str> = frag
            .nodes
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"d0.md") && ids.contains(&"d3.md"));
    }

    #[tokio::test]
    async fn context_overflow_error_also_bisects() {
        let client = Bisecting {
            calls: AtomicUsize::new(0),
            mode: "error",
        };
        let frag = extract_with_adaptive_retry(&client, "sys", docs(3), 5)
            .await
            .unwrap();
        assert_eq!(frag.nodes.len(), 3);
    }

    #[tokio::test]
    async fn single_doc_overflow_yields_empty_not_error() {
        // One doc that always overflows and can't be split -> empty fragment.
        struct AlwaysOverflow;
        #[async_trait]
        impl LlmClient for AlwaysOverflow {
            async fn complete(&self, _s: &str, _u: &str) -> Result<Completion, LlmError> {
                Err(LlmError::Status {
                    status: 400,
                    body: "maximum context length exceeded".into(),
                })
            }
        }
        let frag = extract_with_adaptive_retry(&AlwaysOverflow, "s", docs(1), 3)
            .await
            .unwrap();
        assert!(frag.nodes.is_empty());
    }

    #[tokio::test]
    async fn depth_cap_stops_recursion() {
        // With max_depth 0, a multi-doc truncation can't bisect -> base case.
        let client = Bisecting {
            calls: AtomicUsize::new(0),
            mode: "length",
        };
        let frag = extract_with_adaptive_retry(&client, "s", docs(4), 0)
            .await
            .unwrap();
        // Truncated parse -> empty; exactly one call made (no recursion).
        assert!(frag.nodes.is_empty());
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn hollow_multidoc_response_bisects_to_recover() {
        // A multi-doc chunk returns valid-but-empty JSON with finish_reason
        // "stop" (a "hollow" 200). Single docs return a real node. The hollow
        // response must trigger bisection, recovering all nodes.
        struct HollowOnMulti;
        #[async_trait]
        impl LlmClient for HollowOnMulti {
            async fn complete(&self, _s: &str, user: &str) -> Result<Completion, LlmError> {
                let n = user.matches("<untrusted_source").count();
                if n > 1 {
                    return Ok(Completion {
                        content: "{\"nodes\": [], \"edges\": [], \"hyperedges\": []}".into(),
                        finish_reason: "stop".into(),
                        input_tokens: 0,
                        output_tokens: 0,
                    });
                }
                let id = user
                    .split("path=\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .unwrap_or("x");
                Ok(Completion {
                    content: format!("{{\"nodes\": [{{\"id\": \"{id}\"}}], \"edges\": []}}"),
                    finish_reason: "stop".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                })
            }
        }
        let frag = extract_with_adaptive_retry(&HollowOnMulti, "s", docs(4), 5)
            .await
            .unwrap();
        assert_eq!(
            frag.nodes.len(),
            4,
            "hollow multi-doc bisected to recover all"
        );
    }

    #[tokio::test]
    async fn corpus_isolates_a_failed_chunk() {
        // A hard (non-overflow) error on one doc must not abort the corpus.
        struct FailsOnD1;
        #[async_trait]
        impl LlmClient for FailsOnD1 {
            async fn complete(&self, _s: &str, user: &str) -> Result<Completion, LlmError> {
                if user.contains("path=\"d1.md\"") {
                    return Err(LlmError::Status {
                        status: 500,
                        body: "server boom".into(),
                    });
                }
                let id = user
                    .split("path=\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .unwrap_or("x");
                Ok(Completion {
                    content: format!("{{\"nodes\": [{{\"id\": \"{id}\"}}], \"edges\": []}}"),
                    finish_reason: "stop".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                })
            }
        }
        // Tiny budget -> one doc per chunk, so the d1 failure is isolated.
        let frag = extract_corpus(&FailsOnD1, "s", docs(3), 1, 3, 4)
            .await
            .unwrap();
        assert_eq!(frag.nodes.len(), 2, "d0 and d2 survive; d1 skipped");
    }

    #[tokio::test]
    async fn corpus_chunks_then_extracts() {
        let client = Bisecting {
            calls: AtomicUsize::new(0),
            mode: "length",
        };
        // Tiny budget forces one-doc chunks; each succeeds directly.
        let frag = extract_corpus(&client, "s", docs(3), 1, 3, 4)
            .await
            .unwrap();
        assert_eq!(frag.nodes.len(), 3);
    }

    /// Records the peak number of simultaneous in-flight completions.
    struct ConcurClient {
        inflight: std::sync::Arc<AtomicUsize>,
        peak: std::sync::Arc<AtomicUsize>,
    }
    #[async_trait]
    impl LlmClient for ConcurClient {
        async fn complete(&self, _s: &str, _u: &str) -> Result<Completion, LlmError> {
            let n = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            Ok(Completion {
                content: "{\"nodes\": [{\"id\": \"x\"}], \"edges\": []}".into(),
                finish_reason: "stop".into(),
                input_tokens: 1,
                output_tokens: 1,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn corpus_runs_chunks_concurrently() {
        // H4: with a concurrency cap > 1, independent chunks overlap.
        let peak = std::sync::Arc::new(AtomicUsize::new(0));
        let client = ConcurClient {
            inflight: std::sync::Arc::new(AtomicUsize::new(0)),
            peak: peak.clone(),
        };
        // Tiny budget -> one doc per chunk -> 4 independent chunks.
        let frag = extract_corpus(&client, "s", docs(4), 1, 0, 4)
            .await
            .unwrap();
        assert_eq!(frag.nodes.len(), 4);
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "chunks must run concurrently; peak in-flight = {}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn corpus_concurrency_one_is_sequential() {
        // concurrency = 1 (the ollama / claude-cli setting): never overlaps.
        let peak = std::sync::Arc::new(AtomicUsize::new(0));
        let client = ConcurClient {
            inflight: std::sync::Arc::new(AtomicUsize::new(0)),
            peak: peak.clone(),
        };
        let _ = extract_corpus(&client, "s", docs(4), 1, 0, 1)
            .await
            .unwrap();
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "concurrency=1 must run chunks one at a time"
        );
    }
}
