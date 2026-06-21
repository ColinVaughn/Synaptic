//! LLM dedup tiebreaker: ask the model, in
//! batches, whether each ambiguous concept pair is the same real-world concept,
//! and return the confirmed merges. The caller applies them via
//! `synaptic_graph::merge_pairs`. Keeping this here (not in `synaptic-graph`)
//! preserves the dep DAG — `graph` never depends on `llm`.

use synaptic_core::{Node, NodeId};
use synaptic_llm::LlmClient;

const BATCH_SIZE: usize = 30;
const TIEBREAK_SYSTEM: &str =
    "You decide whether two labels name the same real-world concept. Answer only yes or no per pair.";

/// Resolve ambiguous concept `pairs` via the LLM; returns the confirmed ("yes")
/// pairs. A batch whose request errors is skipped (no merges from it).
pub async fn llm_tiebreak(
    client: &dyn LlmClient,
    nodes: &[Node],
    pairs: Vec<(NodeId, NodeId)>,
) -> Vec<(NodeId, NodeId)> {
    if pairs.is_empty() {
        return vec![];
    }
    let label = |id: &NodeId| {
        nodes
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| id.0.clone())
    };

    let mut confirmed = Vec::new();
    for batch in pairs.chunks(BATCH_SIZE) {
        let listing: Vec<String> = batch
            .iter()
            .enumerate()
            .map(|(i, (a, b))| format!("{}. \"{}\" vs \"{}\"", i + 1, label(a), label(b)))
            .collect();
        let prompt = format!(
            "For each pair below, answer only 'yes' or 'no': are they the same real-world concept?\n\n{}\n\nReply with one line per pair: '1. yes', '2. no', etc.",
            listing.join("\n")
        );
        let Ok(resp) = client.complete(TIEBREAK_SYSTEM, &prompt).await else {
            continue;
        };
        for line in resp.content.lines() {
            let line = line.trim();
            let Some((num, ans)) = line.split_once('.') else {
                continue;
            };
            let Ok(idx) = num.trim().parse::<usize>() else {
                continue;
            };
            if (1..=batch.len()).contains(&idx) && ans.trim().to_lowercase().starts_with("yes") {
                confirmed.push(batch[idx - 1].clone());
            }
        }
    }
    confirmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Map;
    use synaptic_core::FileType;
    use synaptic_llm::{Completion, LlmError};

    struct YesNoMock;

    #[async_trait]
    impl LlmClient for YesNoMock {
        async fn complete(&self, _system: &str, prompt: &str) -> Result<Completion, LlmError> {
            // "yes" to the first pair, "no" to the rest.
            let n = prompt.matches(" vs ").count();
            let mut lines = vec!["1. yes".to_string()];
            for i in 2..=n {
                lines.push(format!("{i}. no"));
            }
            Ok(Completion {
                content: lines.join("\n"),
                finish_reason: "stop".into(),
                input_tokens: 0,
                output_tokens: 0,
            })
        }
    }

    fn node(id: &str, label: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Concept,
            source_file: "d.md".into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    #[tokio::test]
    async fn confirms_only_yes_pairs() {
        let nodes = vec![
            node("a", "Auth"),
            node("b", "Authn"),
            node("c", "Cache"),
            node("d", "Caching"),
        ];
        let pairs = vec![
            (NodeId("a".into()), NodeId("b".into())),
            (NodeId("c".into()), NodeId("d".into())),
        ];
        let confirmed = llm_tiebreak(&YesNoMock, &nodes, pairs).await;
        assert_eq!(confirmed, vec![(NodeId("a".into()), NodeId("b".into()))]);
    }

    #[tokio::test]
    async fn empty_pairs_short_circuits() {
        let confirmed = llm_tiebreak(&YesNoMock, &[], vec![]).await;
        assert!(confirmed.is_empty());
    }
}
