//! .NET project-file extractor (feature `lang-dotnet`). Handles
//! `.csproj` / `.sln` / `.slnx` project files.
//!
//! These are not source languages, so there is no tree-sitter grammar:
//! `.csproj/.fsproj/.vbproj/.slnx` are XML (parsed with `roxmltree`), `.sln` is
//! the legacy line format (parsed with regex). Emitted graph shape:
//! - project file → `contains` → project (`.sln`/`.slnx`)
//! - project file → `imports` → `<ProjectReference>` / `<BuildDependency>` (project)
//! - project file → `imports` → `<PackageReference>` (NuGet, `code`)
//! - project file → `references` → `<TargetFramework(s)>` / SDK (`concept`)
//!
//! Project-reference targets use the **root-relative resolved path** as their id
//! (`file_node_id`), so a reference lands on the referenced project's own file
//! node when it is in the corpus. Synaptic keys file nodes by their portable
//! relative ids (see [`crate::paths::resolve_relative_path`]).

#[cfg(feature = "lang-dotnet")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "lang-dotnet")]
use std::sync::LazyLock;

#[cfg(feature = "lang-dotnet")]
use regex::Regex;
#[cfg(feature = "lang-dotnet")]
use synaptic_core::{make_id, FileType, NodeId};

#[cfg(feature = "lang-dotnet")]
use crate::common::Builder;
#[cfg(feature = "lang-dotnet")]
use crate::paths::{file_node_id, resolve_relative_path};
#[cfg(feature = "lang-dotnet")]
use crate::result::ExtractionResult;

// `.sln` line-format patterns, compiled once process-wide (not per file). M1.
#[cfg(feature = "lang-dotnet")]
static SLN_PROJECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)Project\("[^"]*"\)\s*=\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*"([^"]*)""#)
        .expect("valid sln project regex")
});
#[cfg(feature = "lang-dotnet")]
static SLN_DEP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\{([0-9A-Fa-f-]+)\}\s*=\s*\{([0-9A-Fa-f-]+)\}")
        .expect("valid sln dependency regex")
});

/// Reject project files larger than 2 MB.
#[cfg(feature = "lang-dotnet")]
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Extract a .NET project/solution file, dispatching by extension.
#[cfg(feature = "lang-dotnet")]
pub fn extract_dotnet_source(path: &str, source: &[u8]) -> ExtractionResult {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "sln" => extract_sln(path, source),
        "slnx" => extract_slnx(path, source),
        _ => extract_csproj(path, source), // csproj / fsproj / vbproj
    }
}

/// Read and extract a .NET project file from disk.
#[cfg(feature = "lang-dotnet")]
pub fn extract_dotnet_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_dotnet_source(&path_str, &source))
}

#[cfg(feature = "lang-dotnet")]
fn filename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// A builder seeded with the project-file's own `code` file node.
#[cfg(feature = "lang-dotnet")]
fn builder_with_file(path: &str) -> (Builder, NodeId) {
    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    b.add_node(file_nid.clone(), filename(path), 1);
    (b, file_nid)
}

/// XXE / billion-laughs screen + size cap.
#[cfg(feature = "lang-dotnet")]
fn xml_is_safe(text: &str) -> bool {
    if text.len() > MAX_BYTES {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    !lower.contains("<!doctype") && !lower.contains("<!entity")
}

/// Case-insensitive attribute lookup (MSBuild accepts both `Include`/`include`).
#[cfg(feature = "lang-dotnet")]
fn attr_ci<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name().eq_ignore_ascii_case(name))
        .map(|a| a.value())
}

/// 1-based line of an XML node.
#[cfg(feature = "lang-dotnet")]
fn xml_line(doc: &roxmltree::Document, node: roxmltree::Node) -> usize {
    doc.text_pos_at(node.range().start).row as usize
}

#[cfg(feature = "lang-dotnet")]
fn add_framework(b: &mut Builder, file_nid: &NodeId, fw: &str, line: usize) {
    let nid = NodeId(make_id(&["framework", fw]));
    b.add_node_typed(nid.clone(), fw.to_string(), FileType::Concept, line);
    b.add_edge(
        file_nid.clone(),
        nid,
        "references",
        line,
        Some("target_framework"),
    );
}

/// `.csproj` / `.fsproj` / `.vbproj` — MSBuild XML.
#[cfg(feature = "lang-dotnet")]
fn extract_csproj(path: &str, source: &[u8]) -> ExtractionResult {
    let (mut b, file_nid) = builder_with_file(path);
    let Ok(text) = std::str::from_utf8(source) else {
        return b.into_result();
    };
    if !xml_is_safe(text) {
        return b.into_result();
    }
    let Ok(doc) = roxmltree::Document::parse(text) else {
        return b.into_result();
    };
    let root = doc.root_element();

    // SDK-style `<Project Sdk="Microsoft.NET.Sdk[;...]">` becomes a concept.
    if let Some(sdk) = attr_ci(root, "Sdk") {
        let line = xml_line(&doc, root);
        for s in sdk.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let nid = NodeId(make_id(&["sdk", s]));
            b.add_node_typed(nid.clone(), s.to_string(), FileType::Concept, line);
            b.add_edge(file_nid.clone(), nid, "references", line, Some("sdk"));
        }
    }

    for node in doc.descendants().filter(roxmltree::Node::is_element) {
        let line = xml_line(&doc, node);
        match node.tag_name().name() {
            "TargetFramework" => {
                if let Some(t) = node.text().map(str::trim).filter(|t| !t.is_empty()) {
                    add_framework(&mut b, &file_nid, t, line);
                }
            }
            "TargetFrameworks" => {
                if let Some(t) = node.text() {
                    for fw in t.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                        add_framework(&mut b, &file_nid, fw, line);
                    }
                }
            }
            "PackageReference" => {
                if let Some(name) = attr_ci(node, "Include")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let label = match attr_ci(node, "Version").map(str::trim) {
                        Some(v) if !v.is_empty() => format!("{name} ({v})"),
                        _ => name.to_string(),
                    };
                    let nid = NodeId(make_id(&["nuget", name]));
                    b.add_node(nid.clone(), label, line);
                    b.add_edge(file_nid.clone(), nid, "imports", line, Some("package"));
                }
            }
            "ProjectReference" => {
                if let Some(inc) = attr_ci(node, "Include")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let resolved = resolve_relative_path(path, inc);
                    let nid = file_node_id(&resolved);
                    b.add_external_node(nid.clone(), filename(&resolved));
                    b.add_edge(file_nid.clone(), nid, "imports", line, Some("project"));
                }
            }
            _ => {}
        }
    }
    b.into_result()
}

/// `.sln` — legacy line-based solution format (regex).
#[cfg(feature = "lang-dotnet")]
fn extract_sln(path: &str, source: &[u8]) -> ExtractionResult {
    let (mut b, file_nid) = builder_with_file(path);
    let text = String::from_utf8_lossy(source);

    // Project("{type-guid}") = "Name", "relative\path.csproj", "{project-guid}"
    let proj_re = &*SLN_PROJECT_RE;
    // A ProjectDependencies entry: `{guid} = {guid}` (both the dependency's guid).
    let dep_re = &*SLN_DEP_RE;

    let strip_braces = |g: &str| {
        g.trim_matches(|c| c == '{' || c == '}')
            .to_ascii_lowercase()
    };

    // Pass 1: all projects (so every dependency guid can resolve).
    let mut guid_to_nid: HashMap<String, NodeId> = HashMap::new();
    for cap in proj_re.captures_iter(&text) {
        let name = cap[1].trim();
        let rel = cap[2].trim();
        let guid = strip_braces(&cap[3]);
        let resolved = resolve_relative_path(path, rel);
        let nid = file_node_id(&resolved);
        b.add_external_node(nid.clone(), name.to_string());
        b.add_edge(
            file_nid.clone(),
            nid.clone(),
            "contains",
            1,
            Some("project"),
        );
        if !guid.is_empty() {
            guid_to_nid.insert(guid, nid);
        }
    }

    // Pass 2: inter-project dependencies inside ProjectSection(ProjectDependencies).
    let mut current: Option<NodeId> = None;
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(cap) = proj_re.captures(trimmed) {
            current = guid_to_nid.get(&strip_braces(&cap[3])).cloned();
            in_deps = false;
            continue;
        }
        if trimmed.eq_ignore_ascii_case("EndProject") {
            current = None;
            in_deps = false;
            continue;
        }
        if trimmed.starts_with("ProjectSection(ProjectDependencies)") {
            in_deps = true;
            continue;
        }
        if trimmed.starts_with("EndProjectSection") {
            in_deps = false;
            continue;
        }
        if in_deps {
            if let (Some(cap), Some(from)) = (dep_re.captures(trimmed), &current) {
                if let Some(to) = guid_to_nid.get(&cap[1].to_ascii_lowercase()) {
                    if from != to {
                        b.add_edge(
                            from.clone(),
                            to.clone(),
                            "imports",
                            1,
                            Some("project_dependency"),
                        );
                    }
                }
            }
        }
    }
    b.into_result()
}

/// `.slnx` — the modern XML solution format (no GUIDs).
#[cfg(feature = "lang-dotnet")]
fn extract_slnx(path: &str, source: &[u8]) -> ExtractionResult {
    let (mut b, file_nid) = builder_with_file(path);
    let Ok(text) = std::str::from_utf8(source) else {
        return b.into_result();
    };
    if !xml_is_safe(text) {
        return b.into_result();
    }
    let Ok(doc) = roxmltree::Document::parse(text) else {
        return b.into_result();
    };

    let mut project_nids: HashSet<NodeId> = HashSet::new();
    let mut projects: Vec<(NodeId, roxmltree::Node)> = Vec::new();
    for node in doc.descendants().filter(roxmltree::Node::is_element) {
        if node.tag_name().name() != "Project" {
            continue;
        }
        if let Some(p) = attr_ci(node, "Path")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let resolved = resolve_relative_path(path, p);
            let label = std::path::Path::new(&resolved)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| resolved.clone());
            let nid = file_node_id(&resolved);
            let line = xml_line(&doc, node);
            b.add_external_node(nid.clone(), label);
            b.add_edge(
                file_nid.clone(),
                nid.clone(),
                "contains",
                line,
                Some("project"),
            );
            project_nids.insert(nid.clone());
            projects.push((nid, node));
        }
    }

    for (proj_nid, pnode) in &projects {
        // Match `BuildDependency` at any depth under the project, not just
        // direct children.
        for child in pnode.descendants().filter(roxmltree::Node::is_element) {
            if child.tag_name().name() != "BuildDependency" {
                continue;
            }
            if let Some(dep) = attr_ci(child, "Project")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let resolved = resolve_relative_path(path, dep);
                let nid = file_node_id(&resolved);
                if project_nids.contains(&nid) && nid != *proj_nid {
                    let line = xml_line(&doc, child);
                    b.add_edge(
                        proj_nid.clone(),
                        nid,
                        "imports",
                        line,
                        Some("project_dependency"),
                    );
                }
            }
        }
    }
    b.into_result()
}

#[cfg(all(test, feature = "lang-dotnet"))]
mod tests {
    use super::*;

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &NodeId| {
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

    fn node_type(r: &ExtractionResult, label: &str) -> Option<FileType> {
        r.nodes
            .iter()
            .find(|n| n.label == label)
            .map(|n| n.file_type)
    }

    const CSPROJ: &[u8] = br#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
    <ProjectReference Include="..\Lib\Lib.csproj" />
  </ItemGroup>
</Project>
"#;

    #[test]
    fn csproj_target_framework_is_a_concept_referenced() {
        let r = extract_dotnet_source("src/App/App.csproj", CSPROJ);
        assert!(
            rels(&r, "references").contains(&("App.csproj".to_string(), "net8.0".to_string())),
            "{:?}",
            r.edges
        );
        assert_eq!(node_type(&r, "net8.0"), Some(FileType::Concept));
    }

    #[test]
    fn csproj_sdk_is_a_concept_referenced() {
        let r = extract_dotnet_source("src/App/App.csproj", CSPROJ);
        assert!(rels(&r, "references")
            .contains(&("App.csproj".to_string(), "Microsoft.NET.Sdk".to_string())));
        assert_eq!(node_type(&r, "Microsoft.NET.Sdk"), Some(FileType::Concept));
    }

    #[test]
    fn csproj_package_reference_is_a_nuget_import() {
        let r = extract_dotnet_source("src/App/App.csproj", CSPROJ);
        let imports = rels(&r, "imports");
        assert!(
            imports.contains(&(
                "App.csproj".to_string(),
                "Newtonsoft.Json (13.0.1)".to_string()
            )),
            "{imports:?}"
        );
        assert_eq!(
            node_type(&r, "Newtonsoft.Json (13.0.1)"),
            Some(FileType::Code)
        );
    }

    #[test]
    fn csproj_project_reference_resolves_to_sibling_file_node() {
        let r = extract_dotnet_source("src/App/App.csproj", CSPROJ);
        // ..\Lib\Lib.csproj from src/App resolves to src/Lib/Lib.csproj; its node id
        // must equal that file's own file_node_id so a real corpus merge connects them.
        let want = file_node_id("src/Lib/Lib.csproj");
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "imports" && e.target == want),
            "no import edge to src/Lib/Lib.csproj: {:?}",
            r.edges
        );
    }

    #[test]
    fn csproj_multiple_target_frameworks_split() {
        let src = br#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><TargetFrameworks>net8.0;net9.0</TargetFrameworks></PropertyGroup>
</Project>"#;
        let r = extract_dotnet_source("a/A.csproj", src);
        let refs = rels(&r, "references");
        assert!(refs.iter().any(|(_, t)| t == "net8.0"), "{refs:?}");
        assert!(refs.iter().any(|(_, t)| t == "net9.0"), "{refs:?}");
    }

    #[test]
    fn csproj_rejects_xxe_doctype() {
        let evil = br#"<?xml version="1.0"?><!DOCTYPE foo [<!ENTITY x "y">]>
<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#;
        let r = extract_dotnet_source("a/A.csproj", evil);
        // Only the file node survives; nothing parsed out of the unsafe doc.
        assert_eq!(r.nodes.len(), 1, "{:?}", r.nodes);
    }

    const SLN: &[u8] = br#"
Microsoft Visual Studio Solution File, Format Version 12.00
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "App", "App\App.csproj", "{11111111-1111-1111-1111-111111111111}"
	ProjectSection(ProjectDependencies) = postProject
		{22222222-2222-2222-2222-222222222222} = {22222222-2222-2222-2222-222222222222}
	EndProjectSection
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Lib", "Lib\Lib.csproj", "{22222222-2222-2222-2222-222222222222}"
EndProject
"#;

    #[test]
    fn sln_projects_are_contained() {
        let r = extract_dotnet_source("Solution.sln", SLN);
        let contains = rels(&r, "contains");
        assert!(contains.iter().any(|(_, t)| t == "App"), "{contains:?}");
        assert!(contains.iter().any(|(_, t)| t == "Lib"), "{contains:?}");
    }

    #[test]
    fn sln_project_node_id_matches_referenced_csproj() {
        let r = extract_dotnet_source("Solution.sln", SLN);
        // "App\App.csproj" resolves to App/App.csproj; node id must match that file node.
        let want = file_node_id("App/App.csproj");
        assert!(r.nodes.iter().any(|n| n.id == want), "{:?}", r.nodes);
    }

    #[test]
    fn sln_project_dependencies_become_imports() {
        let r = extract_dotnet_source("Solution.sln", SLN);
        // App depends on Lib (guid 2222...), so App imports Lib.
        let app = file_node_id("App/App.csproj");
        let lib = file_node_id("Lib/Lib.csproj");
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "imports" && e.source == app && e.target == lib),
            "{:?}",
            r.edges
        );
    }

    const SLNX: &[u8] = br#"<Solution>
  <Project Path="App/App.csproj">
    <BuildDependency Project="Lib/Lib.csproj" />
  </Project>
  <Project Path="Lib/Lib.csproj" />
</Solution>"#;

    #[test]
    fn slnx_projects_contained_and_dependency_imported() {
        let r = extract_dotnet_source("Solution.slnx", SLNX);
        let contains = rels(&r, "contains");
        assert!(contains.iter().any(|(_, t)| t == "App"), "{contains:?}");
        assert!(contains.iter().any(|(_, t)| t == "Lib"), "{contains:?}");
        let app = file_node_id("App/App.csproj");
        let lib = file_node_id("Lib/Lib.csproj");
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "imports" && e.source == app && e.target == lib),
            "{:?}",
            r.edges
        );
    }
}
