//! YAML extractor — Bucket C (custom, config-only).
//!
//! Only recognized config kinds are extracted (others return empty, to stay
//! precise): CI workflows (`jobs:` with `needs:` → `depends_on`), Docker Compose
//! (`services:` with `depends_on:` → `depends_on`), and Kubernetes manifests
//! (`kind:` → a node). Each `---`-separated document in a stream is processed
//! independently (a k8s file often packs several resources). Generic/data YAML
//! returns empty.

#[cfg(feature = "lang-yaml")]
use std::collections::HashMap;

#[cfg(feature = "lang-yaml")]
use codegraph_core::{make_id, NodeId};
#[cfg(feature = "lang-yaml")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-yaml")]
use crate::common::Builder;
#[cfg(feature = "lang-yaml")]
use crate::paths::file_node_id;
#[cfg(feature = "lang-yaml")]
use crate::result::ExtractionResult;

/// Extract a YAML file already in memory. Empty for unrecognized/data YAML.
#[cfg(feature = "lang-yaml")]
pub fn extract_yaml_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .expect("load tree-sitter-yaml");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let ex = Yaml { src: source };
    // A YAML stream may hold several `---`-separated documents (common in k8s);
    // process each independently.
    let docs = ex.document_mappings(tree.root_node());
    let per_doc: Vec<(TsNode, Vec<TsNode>)> = docs
        .into_iter()
        .map(|m| {
            let pairs = ex.pairs(m);
            (m, pairs)
        })
        .collect();
    let doc_keys = |pairs: &[TsNode]| -> Vec<String> {
        pairs.iter().filter_map(|p| ex.key_text(*p)).collect()
    };
    let is_config = per_doc.iter().any(|(_, pairs)| {
        let keys = doc_keys(pairs);
        keys.iter()
            .any(|k| k == "jobs" || k == "services" || k == "kind")
    });
    if !is_config {
        return ExtractionResult::default();
    }

    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    b.add_node(file_nid.clone(), filename, 1);

    for (idx, (_m, pairs)) in per_doc.iter().enumerate() {
        let keys = doc_keys(pairs);
        let has = |k: &str| keys.iter().any(|x| x == k);
        if has("jobs") {
            ex.extract_dependency_group(&mut b, &file_nid, pairs, "jobs", "needs");
        } else if has("services") {
            ex.extract_dependency_group(&mut b, &file_nid, pairs, "services", "depends_on");
        } else if has("kind") {
            // Kubernetes manifest: one node labeled by `kind` (+ metadata.name).
            // `idx` disambiguates same-kind/no-name docs within a multi-doc file.
            let kind = ex.value_scalar_for(pairs, "kind").unwrap_or_default();
            if !kind.is_empty() {
                let name = ex.nested_scalar(pairs, "metadata", "name");
                let label = match name {
                    Some(n) if !n.is_empty() => format!("{kind}/{n}"),
                    _ => kind.clone(),
                };
                let id = NodeId(make_id(&["k8s", path, &idx.to_string(), &label]));
                b.add_node(id.clone(), label, 1);
                b.add_edge(file_nid.clone(), id, "contains", 1, Some("k8s"));
            }
        }
    }
    b.into_result()
}

/// Read and extract a YAML file from disk.
#[cfg(feature = "lang-yaml")]
pub fn extract_yaml_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_yaml_source(&path_str, &source))
}

#[cfg(feature = "lang-yaml")]
struct Yaml<'a> {
    src: &'a [u8],
}

#[cfg(feature = "lang-yaml")]
impl Yaml<'_> {
    fn text(&self, node: TsNode) -> String {
        node.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn children(node: TsNode) -> Vec<TsNode> {
        let mut c = node.walk();
        node.children(&mut c).collect()
    }

    /// First descendant (BFS) of `node` with the given kind.
    fn first_kind<'t>(&self, node: TsNode<'t>, kind: &str) -> Option<TsNode<'t>> {
        let mut q = std::collections::VecDeque::from([node]);
        while let Some(n) = q.pop_front() {
            if n.kind() == kind {
                return Some(n);
            }
            for c in Self::children(n) {
                q.push_back(c);
            }
        }
        None
    }

    /// The top `block_mapping` of each `---`-separated `document` in the stream
    /// (falls back to the first mapping anywhere if the root isn't a stream).
    fn document_mappings<'t>(&self, root: TsNode<'t>) -> Vec<TsNode<'t>> {
        let mut out = Vec::new();
        for doc in Self::children(root)
            .into_iter()
            .filter(|c| c.kind() == "document")
        {
            if let Some(m) = self.first_kind(doc, "block_mapping") {
                out.push(m);
            }
        }
        if out.is_empty() {
            if let Some(m) = self.first_kind(root, "block_mapping") {
                out.push(m);
            }
        }
        out
    }

    /// `block_mapping_pair` children of a `block_mapping`.
    fn pairs<'t>(&self, mapping: TsNode<'t>) -> Vec<TsNode<'t>> {
        Self::children(mapping)
            .into_iter()
            .filter(|c| c.kind() == "block_mapping_pair")
            .collect()
    }

    /// Unquoted key text of a pair.
    fn key_text(&self, pair: TsNode) -> Option<String> {
        let key = pair.child_by_field_name("key")?;
        Some(self.scalar(key))
    }

    /// Trimmed, unquoted text of a scalar / flow node.
    fn scalar(&self, node: TsNode) -> String {
        self.text(node)
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string()
    }

    /// The value node of the pair whose key is `key`, as a `block_mapping`.
    fn value_mapping<'t>(&self, pairs: &[TsNode<'t>], key: &str) -> Option<TsNode<'t>> {
        let pair = pairs
            .iter()
            .find(|p| self.key_text(**p).as_deref() == Some(key))?;
        let value = pair.child_by_field_name("value")?;
        self.first_kind(value, "block_mapping")
    }

    /// The scalar value of the top-level pair whose key is `key`.
    fn value_scalar_for(&self, pairs: &[TsNode], key: &str) -> Option<String> {
        let pair = pairs
            .iter()
            .find(|p| self.key_text(**p).as_deref() == Some(key))?;
        let value = pair.child_by_field_name("value")?;
        let mut out = Vec::new();
        self.collect_scalars(value, &mut out);
        out.into_iter().next()
    }

    /// `<outer>: { <inner>: <scalar> }` → the inner scalar.
    fn nested_scalar(&self, pairs: &[TsNode], outer: &str, inner: &str) -> Option<String> {
        let m = self.value_mapping(pairs, outer)?;
        self.value_scalar_for(&self.pairs(m), inner)
    }

    /// All scalar leaves under `node` (handles a single scalar or a sequence).
    fn collect_scalars(&self, node: TsNode, out: &mut Vec<String>) {
        if node.kind().ends_with("_scalar") {
            let s = self.scalar(node);
            if !s.is_empty() {
                out.push(s);
            }
            return;
        }
        for c in Self::children(node) {
            self.collect_scalars(c, out);
        }
    }

    /// Extract a group of named entries (CI `jobs`, Compose `services`) as nodes
    /// and turn each entry's `dep_key` (`needs` / `depends_on`) into `depends_on`
    /// edges between entries.
    fn extract_dependency_group(
        &self,
        b: &mut Builder,
        file_nid: &NodeId,
        top_pairs: &[TsNode],
        group_key: &str,
        dep_key: &str,
    ) {
        let Some(group) = self.value_mapping(top_pairs, group_key) else {
            return;
        };
        let entries = self.pairs(group);
        let mut ids: HashMap<String, NodeId> = HashMap::new();
        // Pass 1: a node per entry.
        for e in &entries {
            let Some(name) = self.key_text(*e) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let id = NodeId(make_id(&[group_key, &name]));
            b.add_node(id.clone(), name.clone(), e.start_position().row + 1);
            b.add_edge(file_nid.clone(), id.clone(), "contains", 1, Some(group_key));
            ids.insert(name, id);
        }
        // Pass 2: dependency edges.
        for e in &entries {
            let Some(name) = self.key_text(*e) else {
                continue;
            };
            let Some(src_id) = ids.get(&name).cloned() else {
                continue;
            };
            let Some(value) = e.child_by_field_name("value") else {
                continue;
            };
            let Some(mapping) = self.first_kind(value, "block_mapping") else {
                continue;
            };
            for inner in self.pairs(mapping) {
                if self.key_text(inner).as_deref() != Some(dep_key) {
                    continue;
                }
                let Some(dep_val) = inner.child_by_field_name("value") else {
                    continue;
                };
                let mut deps = Vec::new();
                self.collect_scalars(dep_val, &mut deps);
                let line = inner.start_position().row + 1;
                for dep in deps {
                    let tgt = ids.get(&dep).cloned().unwrap_or_else(|| {
                        let stub = NodeId(make_id(&[group_key, &dep]));
                        b.add_external_node(stub.clone(), dep.clone());
                        stub
                    });
                    b.add_edge(src_id.clone(), tgt, "depends_on", line, Some(dep_key));
                }
            }
        }
    }
}

#[cfg(all(test, feature = "lang-yaml"))]
mod tests {
    use super::extract_yaml_source;
    use crate::result::ExtractionResult;

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &codegraph_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    #[test]
    fn ci_jobs_and_needs_become_depends_on() {
        let src = b"name: CI\njobs:\n  build:\n    runs-on: ubuntu\n  test:\n    needs: build\n  deploy:\n    needs: [build, test]\n";
        let r = extract_yaml_source(".github/workflows/ci.yml", src);
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(labels.contains(&"build".to_string()), "{labels:?}");
        assert!(labels.contains(&"test".to_string()));
        let dep = rels(&r, "depends_on");
        assert!(
            dep.contains(&("test".to_string(), "build".to_string())),
            "{dep:?}"
        );
        assert!(dep.contains(&("deploy".to_string(), "build".to_string())));
        assert!(dep.contains(&("deploy".to_string(), "test".to_string())));
    }

    #[test]
    fn compose_services_depends_on() {
        let src = b"services:\n  web:\n    image: nginx\n    depends_on:\n      - db\n  db:\n    image: postgres\n";
        let r = extract_yaml_source("docker-compose.yml", src);
        let dep = rels(&r, "depends_on");
        assert!(
            dep.contains(&("web".to_string(), "db".to_string())),
            "{dep:?}"
        );
    }

    #[test]
    fn k8s_kind_node() {
        let src = b"apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: api\n";
        let r = extract_yaml_source("deploy.yaml", src);
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(labels.contains(&"Deployment/api".to_string()), "{labels:?}");
    }

    #[test]
    fn multi_document_k8s_stream() {
        // Two `---`-separated docs in one file give two nodes.
        let src = b"apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: api\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: api-svc\n";
        let r = extract_yaml_source("k8s.yaml", src);
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(labels.contains(&"Deployment/api".to_string()), "{labels:?}");
        assert!(labels.contains(&"Service/api-svc".to_string()));
    }

    #[test]
    fn generic_yaml_is_skipped() {
        let r = extract_yaml_source("config.yaml", b"foo: bar\nbaz:\n  qux: 1\n");
        assert!(r.nodes.is_empty() && r.edges.is_empty());
    }
}
