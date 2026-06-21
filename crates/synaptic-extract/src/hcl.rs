//! Terraform / HCL extractor — Bucket C (custom).
//!
//! Blocks → address nodes (`resource "t" "n"` → `t.n`, `variable` → `var.n`,
//! `output` → `output.n`, `module` → `module.n`, `data` → `data.t.n`,
//! `provider` → `provider.n`); attribute interpolations → `references` edges
//! (`depends_on` → `depends_on` edges). Node ids are directory-scoped so
//! cross-file references inside a module resolve.

#[cfg(feature = "lang-hcl")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-hcl")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-hcl")]
use crate::common::Builder;
#[cfg(feature = "lang-hcl")]
use crate::paths::file_node_id;
#[cfg(feature = "lang-hcl")]
use crate::result::ExtractionResult;

/// Interpolation heads that are not references to other blocks.
#[cfg(feature = "lang-hcl")]
const META_HEADS: &[&str] = &["self", "path", "count", "each", "terraform"];

/// Extract a Terraform/HCL file already in memory.
#[cfg(feature = "lang-hcl")]
pub fn extract_hcl_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_hcl::LANGUAGE.into())
        .expect("load tree-sitter-hcl");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let ex = Hcl { src: source };
    // Directory-scoped ids: same-directory files (one Terraform module) share scope.
    let scope = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    b.add_node(file_nid.clone(), filename, 1);

    let Some(body) = ex.first_kind(tree.root_node(), "body") else {
        return b.into_result();
    };
    for block in Hcl::children(body)
        .into_iter()
        .filter(|c| c.kind() == "block")
    {
        ex.handle_block(&mut b, &file_nid, &scope, block);
    }
    b.into_result()
}

/// Read and extract an HCL file from disk.
#[cfg(feature = "lang-hcl")]
pub fn extract_hcl_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_hcl_source(&path_str, &source))
}

#[cfg(feature = "lang-hcl")]
struct Hcl<'a> {
    src: &'a [u8],
}

#[cfg(feature = "lang-hcl")]
impl Hcl<'_> {
    fn text(&self, node: TsNode) -> String {
        node.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn children(node: TsNode) -> Vec<TsNode> {
        let mut c = node.walk();
        node.children(&mut c).collect()
    }

    fn first_kind<'t>(&self, node: TsNode<'t>, kind: &str) -> Option<TsNode<'t>> {
        Self::children(node).into_iter().find(|c| c.kind() == kind)
    }

    /// The string content of a `string_lit` (its `template_literal`), unquoted.
    fn string_lit(&self, node: TsNode) -> String {
        Self::children(node)
            .into_iter()
            .find(|c| c.kind() == "template_literal")
            .map(|c| self.text(c))
            .unwrap_or_default()
    }

    /// The `identifier` text directly under a `variable_expr`/`get_attr`.
    fn ident(&self, node: TsNode) -> String {
        Self::children(node)
            .into_iter()
            .find(|c| c.kind() == "identifier")
            .map(|c| self.text(c))
            .unwrap_or_default()
    }

    fn handle_block(&self, b: &mut Builder, file_nid: &NodeId, scope: &str, block: TsNode) {
        let kids = Self::children(block);
        let Some(block_type) = kids.iter().find(|c| c.kind() == "identifier") else {
            return;
        };
        let block_type = self.text(*block_type);
        let labels: Vec<String> = kids
            .iter()
            .filter(|c| c.kind() == "string_lit")
            .map(|c| self.string_lit(*c))
            .collect();
        let inner = kids.iter().find(|c| c.kind() == "body").copied();
        let line = block.start_position().row + 1;

        // `locals` is special: each attribute defines its own `local.<key>` node.
        if block_type == "locals" {
            if let Some(inner) = inner {
                for attr in Self::children(inner)
                    .into_iter()
                    .filter(|c| c.kind() == "attribute")
                {
                    if let Some(key) = self.attr_key(attr) {
                        let addr = format!("local.{key}");
                        let id = NodeId(make_id(&[scope, &addr]));
                        b.add_node(id.clone(), addr, line);
                        b.add_edge(file_nid.clone(), id, "contains", line, Some("hcl"));
                    }
                }
            }
            return;
        }

        let Some(address) = block_address(&block_type, &labels) else {
            return;
        };
        let block_nid = NodeId(make_id(&[scope, &address]));
        b.add_node(block_nid.clone(), address, line);
        b.add_edge(
            file_nid.clone(),
            block_nid.clone(),
            "contains",
            line,
            Some("hcl"),
        );

        if let Some(inner) = inner {
            for attr in Self::children(inner)
                .into_iter()
                .filter(|c| c.kind() == "attribute")
            {
                let key = self.attr_key(attr).unwrap_or_default();
                let relation = if key == "depends_on" {
                    "depends_on"
                } else {
                    "references"
                };
                let Some(value) = self.attr_value(attr) else {
                    continue;
                };
                let mut refs = Vec::new();
                self.collect_refs(value, &mut refs);
                let aline = attr.start_position().row + 1;
                for r in refs {
                    let tgt = NodeId(make_id(&[scope, &r]));
                    b.add_external_node(tgt.clone(), r);
                    b.add_edge(block_nid.clone(), tgt, relation, aline, Some("hcl"));
                }
            }
        }
    }

    /// The attribute key (its leading `identifier`).
    fn attr_key(&self, attr: TsNode) -> Option<String> {
        Self::children(attr)
            .into_iter()
            .find(|c| c.kind() == "identifier")
            .map(|c| self.text(c))
    }

    /// The attribute value (its `expression`).
    fn attr_value<'t>(&self, attr: TsNode<'t>) -> Option<TsNode<'t>> {
        Self::children(attr)
            .into_iter()
            .find(|c| c.kind() == "expression")
    }

    /// Collect block-reference addresses from an expression subtree. A node that
    /// directly contains a `variable_expr` yields one reference (head + attrs);
    /// nested expressions (tuples/objects) recurse.
    fn collect_refs(&self, node: TsNode, out: &mut Vec<String>) {
        let kids = Self::children(node);
        if let Some(ve) = kids.iter().find(|c| c.kind() == "variable_expr") {
            let head = self.ident(*ve);
            let attrs: Vec<String> = kids
                .iter()
                .filter(|c| c.kind() == "get_attr")
                .map(|g| self.ident(*g))
                .collect();
            if let Some(addr) = ref_address(&head, &attrs) {
                out.push(addr);
            }
        }
        for c in kids {
            if matches!(c.kind(), "variable_expr" | "get_attr") {
                continue;
            }
            self.collect_refs(c, out);
        }
    }
}

/// The node address of a block from its type + labels.
#[cfg(feature = "lang-hcl")]
fn block_address(block_type: &str, labels: &[String]) -> Option<String> {
    let l0 = labels.first();
    match block_type {
        "resource" => match (labels.first(), labels.get(1)) {
            (Some(t), Some(n)) => Some(format!("{t}.{n}")),
            _ => None,
        },
        "data" => match (labels.first(), labels.get(1)) {
            (Some(t), Some(n)) => Some(format!("data.{t}.{n}")),
            _ => None,
        },
        "variable" => l0.map(|n| format!("var.{n}")),
        "output" => l0.map(|n| format!("output.{n}")),
        "module" => l0.map(|n| format!("module.{n}")),
        "provider" => l0.map(|n| format!("provider.{n}")),
        "terraform" => None,
        _ => l0.map(|n| format!("{block_type}.{n}")),
    }
}

/// The reference address of an interpolation head + attribute chain, or `None`
/// for meta-heads (`self`, `path`, …).
#[cfg(feature = "lang-hcl")]
fn ref_address(head: &str, attrs: &[String]) -> Option<String> {
    if META_HEADS.contains(&head) {
        return None;
    }
    let first = attrs.first()?;
    match head {
        "data" => Some(match attrs.get(1) {
            Some(second) => format!("data.{first}.{second}"),
            None => format!("data.{first}"),
        }),
        // var/local/module/output -> head.name; a resource type -> type.name.
        _ => Some(format!("{head}.{first}")),
    }
}

#[cfg(all(test, feature = "lang-hcl"))]
mod tests {
    use super::extract_hcl_source;
    use crate::result::ExtractionResult;

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &synaptic_core::NodeId| {
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

    const SRC: &[u8] = b"variable \"ami\" {}\n\nresource \"aws_instance\" \"web\" {\n  ami = var.ami\n  depends_on = [aws_vpc.main]\n}\n\noutput \"ip\" {\n  value = aws_instance.web.private_ip\n}\n";

    #[test]
    fn block_address_nodes() {
        let r = extract_hcl_source("infra/main.tf", SRC);
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(
            labels.contains(&"aws_instance.web".to_string()),
            "{labels:?}"
        );
        assert!(labels.contains(&"var.ami".to_string()));
        assert!(labels.contains(&"output.ip".to_string()));
    }

    #[test]
    fn interpolation_becomes_references() {
        let r = extract_hcl_source("infra/main.tf", SRC);
        let refs = rels(&r, "references");
        // resource references var.ami; output references aws_instance.web
        assert!(
            refs.contains(&("aws_instance.web".to_string(), "var.ami".to_string())),
            "refs: {refs:?}"
        );
        assert!(refs
            .iter()
            .any(|(s, t)| s == "output.ip" && t == "aws_instance.web"));
    }

    #[test]
    fn depends_on_is_its_own_relation() {
        let r = extract_hcl_source("infra/main.tf", SRC);
        let dep = rels(&r, "depends_on");
        assert!(
            dep.contains(&("aws_instance.web".to_string(), "aws_vpc.main".to_string())),
            "dep: {dep:?}"
        );
    }

    #[test]
    fn same_directory_scope_is_stable() {
        // Two files in the same dir produce the same node id for the same address,
        // so cross-file references resolve.
        let a = extract_hcl_source("infra/a.tf", b"resource \"aws_vpc\" \"main\" {}\n");
        let b = extract_hcl_source(
            "infra/b.tf",
            b"output \"x\" {\n  value = aws_vpc.main.id\n}\n",
        );
        let vpc_id = a
            .nodes
            .iter()
            .find(|n| n.label == "aws_vpc.main")
            .map(|n| n.id.clone())
            .unwrap();
        assert!(b
            .edges
            .iter()
            .any(|e| e.target == vpc_id && e.relation == "references"));
    }
}
