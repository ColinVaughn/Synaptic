//! The rule engine: a Rule yields Findings from an AuditCtx (the graph plus an
//! optional repo root for re-reading SQL/source). Rules are registered in
//! [`all_rules`] and grow per category module.
use std::path::Path;

use codegraph_graph::KnowledgeGraph;

use crate::findings::Finding;

pub mod design;
pub mod performance;
pub mod security;

pub struct AuditCtx<'a> {
    pub kg: &'a KnowledgeGraph,
    /// Repo root for re-reading source at a call site (N+1 detection). None ->
    /// source-dependent rules skip.
    pub root: Option<&'a Path>,
}

pub trait Rule {
    fn id(&self) -> &'static str;
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding>;
}

/// Every rule in the catalog. Order is irrelevant (the report re-sorts).
pub fn all_rules() -> Vec<Box<dyn Rule>> {
    let mut rules: Vec<Box<dyn Rule>> = Vec::new();
    security::register(&mut rules);
    performance::register(&mut rules);
    design::register(&mut rules);
    rules
}

/// Query strings on code->table edges, with their source location ("file:Lnn")
/// and the [source_id, target_id] pair. Used by the query-text rules.
pub fn query_snippets(ctx: &AuditCtx) -> Vec<(String, Option<String>, Vec<String>)> {
    let rels = ["queries", "writes_to"];
    ctx.kg
        .edges()
        .filter(|e| rels.contains(&e.relation.as_str()))
        .filter_map(|e| {
            let snip = e.extra.get("sql").and_then(|v| v.as_str())?.to_string();
            let loc = e
                .source_location
                .as_ref()
                .map(|l| format!("{}:{}", e.source_file, l));
            Some((snip, loc, vec![e.source.0.clone(), e.target.0.clone()]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use codegraph_core::GraphData;
    use codegraph_graph::KnowledgeGraph;

    #[test]
    fn audit_runs_on_empty_graph() {
        let kg = KnowledgeGraph::from_graph_data(GraphData::default());
        let r = crate::audit(&kg, &crate::AuditOptions::default());
        assert_eq!(r.findings.len(), 0);
        assert_eq!(r.version, crate::findings::AUDIT_VERSION);
    }
}
