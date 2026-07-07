#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use synaptic_core::{FileType, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_query::{affected_nodes, DEFAULT_AFFECTED_RELATIONS};

pub const READINESS_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    #[default]
    Auto,
    Generic,
    MinecraftFabric,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Profile::Auto,
            "generic" => Profile::Generic,
            "minecraft-fabric" | "minecraft_fabric" | "fabric" => Profile::MinecraftFabric,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Auto => "auto",
            Profile::Generic => "generic",
            Profile::MinecraftFabric => "minecraft-fabric",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl Severity {
    pub fn rank(self) -> u8 {
        match self {
            Severity::Critical => 0,
            Severity::High => 1,
            Severity::Medium => 2,
            Severity::Low => 3,
            Severity::Info => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Info => "info",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "medium" => Severity::Medium,
            "low" => Severity::Low,
            "info" => Severity::Info,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Correctness,
    BuildConfig,
    Maintainability,
    Completeness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Impact {
    pub score: u32,
    pub degree: usize,
    pub affected_count: usize,
    pub generated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub rule_id: String,
    pub severity: Severity,
    pub category: Category,
    pub subsystem: String,
    pub title: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub node_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub evidence: Option<String>,
    pub remediation: String,
    pub confidence: f32,
    pub impact: Impact,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupSummary {
    pub subsystem: String,
    pub count: usize,
    pub highest_severity: Severity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadinessReport {
    pub version: u32,
    pub summary: String,
    pub counts_by_severity: BTreeMap<String, usize>,
    pub groups: Vec<GroupSummary>,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AuditOptions {
    pub root: Option<PathBuf>,
    pub profile: Profile,
    pub min_severity: Option<Severity>,
    pub repo: Option<String>,
}

impl Default for AuditOptions {
    fn default() -> Self {
        Self {
            root: None,
            profile: Profile::Auto,
            min_severity: None,
            repo: None,
        }
    }
}

pub fn audit(kg: &KnowledgeGraph, opts: &AuditOptions) -> ReadinessReport {
    let root = opts.root.as_ref().filter(|p| p.is_dir());
    let profile = resolve_profile(root, opts.profile);
    let mut findings = Vec::new();
    let mut skipped = Vec::new();

    if let Some(root) = root {
        findings.extend(source_findings(kg, root, profile, opts.repo.as_deref()));
        findings.extend(config_findings(kg, root, profile));
    } else {
        skipped.push("source/config checks skipped: no project root was provided".to_string());
    }
    findings.extend(graph_rationale_findings(kg, opts.repo.as_deref()));
    findings.extend(graph_shadow_findings(kg, opts.repo.as_deref()));

    if let Some(min) = opts.min_severity {
        findings.retain(|f| f.severity.rank() <= min.rank());
    }
    ReadinessReport::from_findings(findings, skipped)
}

impl ReadinessReport {
    pub fn from_findings(mut findings: Vec<Finding>, skipped: Vec<String>) -> Self {
        let mut seen = HashSet::new();
        findings.retain(|f| {
            seen.insert((
                f.rule_id.clone(),
                f.location.clone(),
                f.evidence.clone(),
                f.node_ids.join(","),
            ))
        });
        findings.sort_by(|a, b| {
            a.severity
                .rank()
                .cmp(&b.severity.rank())
                .then_with(|| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| b.impact.score.cmp(&a.impact.score))
                .then_with(|| a.rule_id.cmp(&b.rule_id))
                .then_with(|| a.location.cmp(&b.location))
        });

        let mut counts = BTreeMap::new();
        let mut groups: BTreeMap<String, (usize, Severity)> = BTreeMap::new();
        for f in &findings {
            *counts.entry(f.severity.as_str().to_string()).or_insert(0) += 1;
            groups
                .entry(f.subsystem.clone())
                .and_modify(|(count, sev)| {
                    *count += 1;
                    if f.severity.rank() < sev.rank() {
                        *sev = f.severity;
                    }
                })
                .or_insert((1, f.severity));
        }
        let groups = groups
            .into_iter()
            .map(|(subsystem, (count, highest_severity))| GroupSummary {
                subsystem,
                count,
                highest_severity,
            })
            .collect::<Vec<_>>();
        let summary = format!(
            "{} finding(s): {} critical, {} high, {} medium, {} low",
            findings.len(),
            counts.get("critical").copied().unwrap_or(0),
            counts.get("high").copied().unwrap_or(0),
            counts.get("medium").copied().unwrap_or(0),
            counts.get("low").copied().unwrap_or(0),
        );
        Self {
            version: READINESS_VERSION,
            summary,
            counts_by_severity: counts,
            groups,
            findings,
            skipped,
        }
    }
}

pub mod render {
    use super::ReadinessReport;

    pub fn render_markdown(report: &ReadinessReport) -> String {
        let mut out = format!("# Port Readiness Audit\n\n{}\n\n", report.summary);
        if !report.groups.is_empty() {
            out.push_str("## Groups\n\n");
            for g in &report.groups {
                out.push_str(&format!(
                    "- {}: {} finding(s), highest severity `{}`\n",
                    g.subsystem,
                    g.count,
                    g.highest_severity.as_str()
                ));
            }
            out.push('\n');
        }
        for f in &report.findings {
            out.push_str(&format!(
                "## [{}] {} ({})\n\n{}\n\n- subsystem: {}\n- confidence: {:.2}\n- impact: {} (degree {}, affected {}, generated {})\n- where: {}\n- fix: {}\n",
                f.severity.as_str(),
                f.title,
                f.rule_id,
                f.detail,
                f.subsystem,
                f.confidence,
                f.impact.score,
                f.impact.degree,
                f.impact.affected_count,
                f.impact.generated,
                f.location.as_deref().unwrap_or("-"),
                f.remediation
            ));
            if let Some(e) = &f.evidence {
                out.push_str(&format!("- evidence: `{}`\n", e.replace('`', "'")));
            }
            out.push('\n');
        }
        if !report.skipped.is_empty() {
            out.push_str("## Skipped\n\n");
            for s in &report.skipped {
                out.push_str(&format!("- {s}\n"));
            }
        }
        out
    }
}

fn resolve_profile(root: Option<&PathBuf>, requested: Profile) -> Profile {
    match requested {
        Profile::Auto => {
            let Some(root) = root else {
                return Profile::Generic;
            };
            if root.join("src/main/resources/fabric.mod.json").is_file()
                || root.join("fabric.mod.json").is_file()
                || read_to_string(root.join("build.gradle.kts"))
                    .or_else(|| read_to_string(root.join("build.gradle")))
                    .is_some_and(|s| s.contains("fabric-loom") || s.contains("fabric.mod.json"))
            {
                Profile::MinecraftFabric
            } else {
                Profile::Generic
            }
        }
        p => p,
    }
}

fn source_findings(
    kg: &KnowledgeGraph,
    root: &Path,
    profile: Profile,
    repo: Option<&str>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut by_file: HashMap<String, Vec<&Node>> = HashMap::new();
    for n in kg.nodes() {
        if !repo_matches(n, repo) || n.source_file.is_empty() || n.span().is_none() {
            continue;
        }
        by_file.entry(n.source_file.clone()).or_default().push(n);
    }

    let placeholder_re = placeholder_regex();
    let sentinel_return_re = sentinel_return_regex();
    for (graph_path, nodes) in by_file {
        let Some(path) = resolve_source_file(root, &graph_path) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for n in nodes {
            let Some(span) = n.span() else { continue };
            let start = span.start_line.saturating_sub(1) as usize;
            let end = span.end_line.min(lines.len() as u32) as usize;
            let body = lines.get(start..end).unwrap_or(&[]).join("\n");
            if sentinel_return_re.is_match(&body) {
                findings.push(sentinel_return_finding(
                    kg,
                    n,
                    &body,
                    &sentinel_return_re,
                    profile,
                ));
            }
        }
        for (idx, line) in lines.iter().enumerate() {
            let Some(m) = placeholder_re.find(line) else {
                continue;
            };
            let loc = format!("{}:L{}", normalize_path(&graph_path), idx + 1);
            let generated = is_generated_path(&graph_path);
            let severity = if generated {
                Severity::Low
            } else {
                Severity::Medium
            };
            let subsystem = if generated {
                "generated".to_string()
            } else {
                subsystem_for(&graph_path, line)
            };
            findings.push(Finding {
                rule_id: "READY-PLACEHOLDER-001".into(),
                severity,
                category: Category::Completeness,
                subsystem,
                title: "Placeholder or unfinished marker".into(),
                detail: "A placeholder or stub-style marker remains in source or resources.".into(),
                location: Some(loc),
                node_ids: enclosing_node_id(kg, &graph_path, (idx + 1) as u32)
                    .into_iter()
                    .map(|id| id.0)
                    .collect(),
                evidence: Some(trim_evidence(line, m.start())),
                remediation:
                    "Resolve the placeholder or downgrade it to tracked non-blocking rationale."
                        .into(),
                confidence: if generated { 0.55 } else { 0.75 },
                impact: source_impact(kg, &graph_path, (idx + 1) as u32, generated, severity),
            });
        }
    }

    for path in walk_project_files(root) {
        let rel = relative_slash(root, &path);
        if by_extension(
            &rel,
            &[
                "json",
                "json5",
                "mcmeta",
                "gradle",
                "kts",
                "toml",
                "yaml",
                "yml",
                "xml",
                "properties",
                "ini",
                "env",
                "cfg",
                "conf",
            ],
        ) {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for (idx, line) in text.lines().enumerate() {
                let Some(m) = placeholder_re.find(line) else {
                    continue;
                };
                let generated = is_generated_path(&rel);
                findings.push(Finding {
                    rule_id: "READY-PLACEHOLDER-001".into(),
                    severity: if generated { Severity::Low } else { Severity::Medium },
                    category: Category::Completeness,
                    subsystem: if generated {
                        "generated".to_string()
                    } else {
                        subsystem_for(&rel, line)
                    },
                    title: "Placeholder or unfinished marker".into(),
                    detail: "A placeholder or stub-style marker remains in source or resources."
                        .into(),
                    location: Some(format!("{rel}:L{}", idx + 1)),
                    node_ids: Vec::new(),
                    evidence: Some(trim_evidence(line, m.start())),
                    remediation:
                        "Resolve the placeholder or exclude generated artifacts from readiness gating."
                            .into(),
                    confidence: if generated { 0.55 } else { 0.7 },
                    impact: Impact {
                        score: if generated { 8 } else { 30 },
                        degree: 0,
                        affected_count: 0,
                        generated,
                    },
                });
            }
        }
    }

    findings
}

fn graph_rationale_findings(kg: &KnowledgeGraph, repo: Option<&str>) -> Vec<Finding> {
    kg.nodes()
        .filter(|n| repo_matches(n, repo))
        .filter(|n| matches!(n.file_type, FileType::Rationale))
        .filter(|n| {
            let u = n.label.to_ascii_uppercase();
            ["TODO", "FIXME", "HACK", "XXX", "NOTIMPLEMENTED"]
                .iter()
                .any(|m| u.contains(m))
        })
        .map(|n| {
            let generated = is_generated_path(&n.source_file);
            let line = n
                .source_location
                .as_deref()
                .map(|l| format!(":{}", l))
                .unwrap_or_default();
            Finding {
                rule_id: "READY-PLACEHOLDER-001".into(),
                severity: if generated {
                    Severity::Low
                } else {
                    Severity::Medium
                },
                category: Category::Completeness,
                subsystem: if generated {
                    "generated".into()
                } else {
                    subsystem_for(&n.source_file, &n.label)
                },
                title: "Placeholder or unfinished marker".into(),
                detail: "A placeholder rationale node remains in the graph.".into(),
                location: Some(format!("{}{}", normalize_path(&n.source_file), line)),
                node_ids: vec![n.id.0.clone()],
                evidence: Some(n.label.clone()),
                remediation:
                    "Resolve the placeholder or capture a tracked follow-up with clear scope."
                        .into(),
                confidence: 0.7,
                impact: node_impact(kg, n, generated, Severity::Medium),
            }
        })
        .collect()
}

/// Findings for `shadows` edges (a generated resource duplicates a hand-authored
/// one at the same logical path). Graph-only, so it runs even without a source
/// root. Universal: fires for any datagen/codegen setup, not just Minecraft.
fn graph_shadow_findings(kg: &KnowledgeGraph, repo: Option<&str>) -> Vec<Finding> {
    kg.edges()
        .filter(|e| e.relation == "shadows")
        .filter_map(|e| {
            let gen = kg.node(&e.source)?;
            let src = kg.node(&e.target)?;
            if !repo_matches(gen, repo) {
                return None;
            }
            Some(Finding {
                rule_id: "READY-RESOURCE-SHADOW".into(),
                severity: Severity::Medium,
                category: Category::Correctness,
                subsystem: subsystem_for(&gen.source_file, &gen.label),
                title: "Generated resource shadows a source resource".into(),
                detail: format!(
                    "`{}` is generated, but a hand-authored resource at the same logical path already exists. The generated copy may silently win or conflict at build time.",
                    normalize_path(&gen.source_file)
                ),
                location: Some(normalize_path(&gen.source_file)),
                node_ids: vec![gen.id.0.clone(), src.id.0.clone()],
                evidence: Some(normalize_path(&src.source_file)),
                remediation:
                    "Remove the stale hand-authored resource or exclude it from datagen so a single authority owns the resource."
                        .into(),
                confidence: 0.8,
                impact: node_impact(kg, gen, false, Severity::Medium),
            })
        })
        .collect()
}

fn sentinel_return_finding(
    kg: &KnowledgeGraph,
    node: &Node,
    body: &str,
    sentinel_return_re: &Regex,
    profile: Profile,
) -> Finding {
    let method = bare_method_name(&node.label);
    let path = normalize_path(&node.source_file);
    let high_risk =
        profile == Profile::MinecraftFabric && high_risk_framework_method(&method, &path);
    let low_risk = low_risk_nullable_method(&method);
    let severity = if high_risk {
        Severity::High
    } else if low_risk {
        Severity::Low
    } else {
        Severity::Medium
    };
    let rule_id = if high_risk {
        "READY-FRAMEWORK-SENTINEL-RETURN"
    } else {
        "READY-SENTINEL-RETURN"
    };
    let generated = is_generated_path(&node.source_file);
    Finding {
        rule_id: rule_id.into(),
        severity,
        category: Category::Correctness,
        subsystem: subsystem_for(&node.source_file, &node.label),
        title: if high_risk {
            "Framework override returns a sentinel value".into()
        } else {
            "Callable returns a sentinel value".into()
        },
        detail: if high_risk {
            format!(
                "`{method}` matches a lifecycle, registry, serialization, generation, or network-style method. A sentinel return here often blocks load, registry, or runtime flows."
            )
        } else if low_risk {
            format!(
                "`{method}` returns a sentinel value in a method name that is commonly nullable; review intent but treat as lower risk."
            )
        } else {
            format!("`{method}` returns a sentinel value; verify this is intentional for the API contract.")
        },
        location: node
            .source_location
            .as_ref()
            .map(|loc| format!("{path}:{loc}")),
        node_ids: vec![node.id.0.clone()],
        evidence: evidence_line(body, sentinel_return_re),
        remediation: if high_risk {
            "Return the registered codec/serializer/type or implement the required lifecycle behavior."
                .into()
        } else {
            "Replace the null with the correct value, Optional-style API, or an explicit documented contract."
                .into()
        },
        confidence: if high_risk { 0.9 } else { 0.72 },
        impact: node_impact(kg, node, generated, severity),
    }
}

fn config_findings(kg: &KnowledgeGraph, root: &Path, profile: Profile) -> Vec<Finding> {
    if profile != Profile::MinecraftFabric {
        return Vec::new();
    }
    let mut findings = Vec::new();
    let build = read_to_string(root.join("build.gradle.kts"))
        .or_else(|| read_to_string(root.join("build.gradle")))
        .unwrap_or_default();
    if !build.contains("fabric-loom") {
        findings.push(config_finding(
            kg,
            "READY-PROFILE-BUILD-001",
            Severity::High,
            "Expected build plugin not detected",
            "The selected readiness profile expects a build plugin that was not found.",
            "build.gradle(.kts)",
            "Add or repair the build plugin configuration required by the selected profile.",
            0.8,
        ));
    }
    if !(build.contains("VERSION_21") || build.contains("JavaLanguageVersion.of(21)")) {
        findings.push(config_finding(
            kg,
            "READY-PROFILE-RUNTIME-001",
            Severity::Medium,
            "Expected runtime compatibility not detected",
            "The selected readiness profile expects runtime/toolchain compatibility settings that were not found.",
            "build.gradle(.kts)",
            "Set the toolchain or source/target compatibility required by the selected profile.",
            0.7,
        ));
    }

    let fabric_path = ["src/main/resources/fabric.mod.json", "fabric.mod.json"]
        .iter()
        .map(|p| root.join(p))
        .find(|p| p.is_file());
    let Some(fabric_path) = fabric_path else {
        findings.push(config_finding(
            kg,
            "READY-PROFILE-METADATA-001",
            Severity::High,
            "Expected project metadata is missing",
            "The selected readiness profile expects project metadata that was not found.",
            "src/main/resources/fabric.mod.json",
            "Add the expected metadata file or choose a language-neutral readiness profile.",
            0.9,
        ));
        return findings;
    };

    let text = read_to_string(&fabric_path).unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if let Some(mixins) = parsed.get("mixins").and_then(|v| v.as_array()) {
        for mixin in mixins.iter().filter_map(|v| v.as_str()) {
            let p = fabric_path.parent().unwrap_or(root).join(mixin);
            if !p.is_file() {
                findings.push(config_finding(
                    kg,
                    "READY-PROFILE-CONFIG-001",
                    Severity::High,
                    "Referenced profile config is missing",
                    &format!("`{mixin}` is listed in project metadata but was not found."),
                    &relative_slash(root, &p),
                    "Create the referenced config file or remove the stale metadata entry.",
                    0.9,
                ));
            }
        }
    }
    if let Some(access) = parsed.get("accessWidener").and_then(|v| v.as_str()) {
        let p = fabric_path.parent().unwrap_or(root).join(access);
        if !p.is_file() {
            findings.push(config_finding(
                kg,
                "READY-PROFILE-ACCESS-001",
                Severity::High,
                "Referenced access metadata is missing",
                &format!("`{access}` is listed in project metadata but was not found."),
                &relative_slash(root, &p),
                "Create the access widener file or remove the stale metadata entry.",
                0.9,
            ));
        }
    }
    findings
}

#[allow(clippy::too_many_arguments)]
fn config_finding(
    _kg: &KnowledgeGraph,
    rule_id: &str,
    severity: Severity,
    title: &str,
    detail: &str,
    location: &str,
    remediation: &str,
    confidence: f32,
) -> Finding {
    Finding {
        rule_id: rule_id.into(),
        severity,
        category: Category::BuildConfig,
        subsystem: "config".into(),
        title: title.into(),
        detail: detail.into(),
        location: Some(location.into()),
        node_ids: Vec::new(),
        evidence: None,
        remediation: remediation.into(),
        confidence,
        impact: Impact {
            score: severity_base(severity) + 10,
            degree: 0,
            affected_count: 0,
            generated: false,
        },
    }
}

fn node_impact(kg: &KnowledgeGraph, node: &Node, generated: bool, severity: Severity) -> Impact {
    let affected_count = affected_nodes(kg, &node.id, DEFAULT_AFFECTED_RELATIONS, 2).len();
    let degree = kg.degree(&node.id);
    let mut score =
        severity_base(severity) + (degree.min(20) as u32 * 2) + (affected_count.min(30) as u32 * 3);
    if generated {
        score /= 3;
    }
    if node.is_test() {
        score = score.saturating_sub(10);
    }
    Impact {
        score,
        degree,
        affected_count,
        generated,
    }
}

fn source_impact(
    kg: &KnowledgeGraph,
    graph_path: &str,
    line: u32,
    generated: bool,
    severity: Severity,
) -> Impact {
    if let Some(id) = enclosing_node_id(kg, graph_path, line) {
        if let Some(n) = kg.node(&id) {
            return node_impact(kg, n, generated, severity);
        }
    }
    Impact {
        score: if generated {
            8
        } else {
            severity_base(severity)
        },
        degree: 0,
        affected_count: 0,
        generated,
    }
}

fn severity_base(severity: Severity) -> u32 {
    match severity {
        Severity::Critical => 100,
        Severity::High => 75,
        Severity::Medium => 45,
        Severity::Low => 20,
        Severity::Info => 5,
    }
}

fn enclosing_node_id(kg: &KnowledgeGraph, graph_path: &str, line: u32) -> Option<NodeId> {
    kg.nodes()
        .filter(|n| n.source_file == graph_path)
        .filter_map(|n| n.span().map(|s| (n, s)))
        .filter(|(_, s)| s.start_line <= line && line <= s.end_line)
        .min_by_key(|(_, s)| s.end_line.saturating_sub(s.start_line))
        .map(|(n, _)| n.id.clone())
}

fn high_risk_framework_method(method: &str, path: &str) -> bool {
    let m = method.to_ascii_lowercase();
    m == "codec"
        || m.contains("serializer")
        || m.contains("streamcodec")
        || m.contains("network")
        || m.contains("registry")
        || m.contains("type")
        || (path.contains("/world/") && (m.contains("generator") || m == "gettypenamefordatafixer"))
}

fn placeholder_regex() -> Regex {
    Regex::new(
        r#"(?i)\b(TODO|FIXME|HACK|XXX|NotImplemented|NotImplementedError|UnsupportedOperationException)\b|(?:todo|unimplemented)\s*!\s*\(|throw\s+new\s+Error\s*\(\s*['"]TODO|raise\s+NotImplementedError\b"#,
    )
    .expect("readiness placeholder regex")
}

fn sentinel_return_regex() -> Regex {
    Regex::new(r"(?i)\breturn\s+(null|undefined|none|nil|nullptr)\b\s*;?|\b=>\s*(null|undefined)\b")
        .expect("readiness sentinel return regex")
}

fn evidence_line(body: &str, pattern: &Regex) -> Option<String> {
    body.lines()
        .find(|line| pattern.is_match(line))
        .map(|line| line.trim().to_string())
}

fn low_risk_nullable_method(method: &str) -> bool {
    let m = method.to_ascii_lowercase();
    m.contains("passenger")
        || m.contains("menuprovider")
        || m.contains("texturelocation")
        || m.starts_with("gettexture")
}

fn bare_method_name(label: &str) -> String {
    label
        .trim_start_matches('.')
        .trim_end_matches("()")
        .split('(')
        .next()
        .unwrap_or(label)
        .to_string()
}

fn subsystem_for(path: &str, text: &str) -> String {
    let s = format!("{} {}", path, text)
        .to_ascii_lowercase()
        .replace('\\', "/");
    for (needle, subsystem) in [
        ("world/gen", "worldgen"),
        ("chunkgenerator", "worldgen"),
        ("rocket", "rocket"),
        ("celestial", "progression"),
        ("research", "progression"),
        ("machine", "machines"),
        ("compat", "compat"),
        ("mixin", "compat"),
        ("render", "rendering"),
        ("model", "rendering"),
        ("data/", "datagen"),
        ("datagen", "datagen"),
        ("generated", "generated"),
        ("gradle", "config"),
        ("fabric.mod.json", "config"),
    ] {
        if s.contains(needle) {
            return subsystem.into();
        }
    }
    "other".into()
}

fn is_generated_path(path: &str) -> bool {
    let p = path.replace('\\', "/").to_ascii_lowercase();
    p.contains("/generated/")
        || p.starts_with("generated/")
        || p.contains("/build/")
        || p.contains("/data/generated/")
        || p.contains("/recipes/generated/")
}

fn repo_matches(n: &Node, repo: Option<&str>) -> bool {
    repo.is_none_or(|r| n.repo.as_deref() == Some(r) || n.source_file.starts_with(&format!("{r}/")))
}

fn resolve_source_file(root: &Path, graph_path: &str) -> Option<PathBuf> {
    let rel = graph_path.replace('\\', "/");
    let direct = root.join(&rel);
    if direct.is_file() {
        return Some(direct);
    }
    if let Some((_, tail)) = rel.split_once('/') {
        let p = root.join(tail);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn walk_project_files(root: &Path) -> Vec<PathBuf> {
    fn rec(out: &mut Vec<PathBuf>, dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !matches!(
                    name.as_str(),
                    ".git" | ".gradle" | "build" | "target" | "node_modules" | "synaptic-out"
                ) {
                    rec(out, &p);
                }
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    rec(&mut out, root);
    out
}

fn by_extension(path: &str, exts: &[&str]) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn relative_slash(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn read_to_string(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn trim_evidence(line: &str, start: usize) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= 180 {
        return trimmed.to_string();
    }
    let prefix = line[..start.min(line.len())]
        .chars()
        .count()
        .saturating_sub(40);
    let mut s: String = trimmed.chars().skip(prefix).take(180).collect();
    s.push_str("...");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Edge, GraphData, Node, NodeKind, Span};

    fn node(id: &str, label: &str, file: &str, start: u32, end: u32) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some(format!("L{start}")),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        n.set_kind(NodeKind::Method);
        n.set_span(Span {
            start_line: start,
            end_line: end,
            start_col: 1,
            end_col: 1,
        });
        n
    }

    fn kg(nodes: Vec<Node>, edges: Vec<Edge>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: edges,
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    #[test]
    fn profile_codec_sentinel_return_is_high_risk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/domain/world/gen/Generator.kt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            "class Generator {\n  fun codec(): Any? {\n    return null\n  }\n}\n",
        )
        .unwrap();
        let n = node(
            "codec",
            ".codec()",
            "src/domain/world/gen/Generator.kt",
            2,
            4,
        );
        let graph = kg(vec![n], vec![]);
        let report = audit(
            &graph,
            &AuditOptions {
                root: Some(dir.path().into()),
                profile: Profile::MinecraftFabric,
                ..AuditOptions::default()
            },
        );
        let f = report
            .findings
            .iter()
            .find(|f| f.rule_id == "READY-FRAMEWORK-SENTINEL-RETURN")
            .unwrap();
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.node_ids, vec!["codec"]);
    }

    #[test]
    fn nullable_passenger_method_is_lower_severity() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/entities/RocketEntity.ts");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            "export function getControllingPassenger() {\n  return null;\n}\n",
        )
        .unwrap();
        let graph = kg(
            vec![node(
                "p",
                ".getControllingPassenger()",
                "src/entities/RocketEntity.ts",
                1,
                3,
            )],
            vec![],
        );
        let report = audit(
            &graph,
            &AuditOptions {
                root: Some(dir.path().into()),
                profile: Profile::Generic,
                ..AuditOptions::default()
            },
        );
        let f = report
            .findings
            .iter()
            .find(|f| f.rule_id == "READY-SENTINEL-RETURN")
            .unwrap();
        assert_eq!(f.severity, Severity::Low);
    }

    #[test]
    fn generic_audit_flags_stubs_across_supported_source_files() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "py",
                "src/app.py",
                "def load():\n    raise NotImplementedError()\n",
            ),
            (
                "ts",
                "src/app.ts",
                "export function load() {\n  return undefined;\n}\n",
            ),
            ("rs", "src/lib.rs", "pub fn load() {\n    todo!()\n}\n"),
            ("go", "src/app.go", "func load() any {\n  return nil\n}\n"),
            ("cs", "src/App.cs", "object Load() {\n  return null;\n}\n"),
            (
                "cpp",
                "src/app.cpp",
                "void* load() {\n  return nullptr;\n}\n",
            ),
            (
                "php",
                "src/app.php",
                "<?php\nfunction load() {\n  return null;\n}\n",
            ),
            ("rb", "src/app.rb", "def load\n  return nil\nend\n"),
            (
                "swift",
                "src/App.swift",
                "func load() -> String? {\n  return nil\n}\n",
            ),
            (
                "kt",
                "src/App.kt",
                "fun load(): String {\n  TODO(\"wire\")\n}\n",
            ),
            (
                "sh",
                "scripts/build.sh",
                "build() {\n  # FIXME wire this\n}\n",
            ),
        ];
        let mut nodes = Vec::new();
        for (id, rel, text) in cases {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            nodes.push(node(id, "load", rel, 1, text.lines().count() as u32));
        }
        let report = audit(
            &kg(nodes, vec![]),
            &AuditOptions {
                root: Some(dir.path().into()),
                profile: Profile::Generic,
                ..AuditOptions::default()
            },
        );
        for id in [
            "py", "ts", "rs", "go", "cs", "cpp", "php", "rb", "swift", "kt", "sh",
        ] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.node_ids.iter().any(|n| n == id)),
                "missing finding linked to {id}: {:?}",
                report.findings
            );
        }
        assert!(report
            .findings
            .iter()
            .any(|f| f.rule_id == "READY-SENTINEL-RETURN"));
        assert!(report
            .findings
            .iter()
            .any(|f| f.rule_id == "READY-PLACEHOLDER-001"));
    }

    #[test]
    fn placeholders_group_and_generated_is_downranked() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src/content/rocket/Rocket.ts");
        let gen = dir
            .path()
            .join("src/main/generated/data/mod/recipe/pipe.json");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::create_dir_all(gen.parent().unwrap()).unwrap();
        fs::write(&src, "class R {\n // TODO launch flow\n}\n").unwrap();
        fs::write(&gen, "{ \"pattern\": [\"XXX\"] }\n").unwrap();
        let graph = kg(
            vec![node(
                "rocket",
                "Rocket",
                "src/content/rocket/Rocket.ts",
                1,
                3,
            )],
            vec![],
        );
        let report = audit(
            &graph,
            &AuditOptions {
                root: Some(dir.path().into()),
                ..AuditOptions::default()
            },
        );
        assert!(report.groups.iter().any(|g| g.subsystem == "rocket"));
        assert!(report
            .findings
            .iter()
            .any(|f| f.subsystem == "generated" && f.impact.generated));
        let generated = report
            .findings
            .iter()
            .find(|f| f.subsystem == "generated")
            .unwrap();
        let rocket = report
            .findings
            .iter()
            .find(|f| f.subsystem == "rocket")
            .unwrap();
        assert!(rocket.impact.score > generated.impact.score);
    }

    #[test]
    fn missing_profile_references_create_config_findings() {
        let dir = tempfile::tempdir().unwrap();
        let res = dir.path().join("src/main/resources");
        fs::create_dir_all(&res).unwrap();
        fs::write(
            res.join("fabric.mod.json"),
            r#"{ "mixins": ["missing.mixins.json"], "accessWidener": "missing.accesswidener" }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("build.gradle.kts"),
            "plugins { id(\"fabric-loom\") }\n",
        )
        .unwrap();
        let report = audit(
            &kg(vec![], vec![]),
            &AuditOptions {
                root: Some(dir.path().into()),
                profile: Profile::MinecraftFabric,
                ..AuditOptions::default()
            },
        );
        assert!(report
            .findings
            .iter()
            .any(|f| f.rule_id == "READY-PROFILE-CONFIG-001"));
        assert!(report
            .findings
            .iter()
            .any(|f| f.rule_id == "READY-PROFILE-ACCESS-001"));
    }

    #[test]
    fn findings_sort_by_severity_confidence_and_impact() {
        let low = Finding {
            rule_id: "low".into(),
            severity: Severity::Low,
            category: Category::Completeness,
            subsystem: "other".into(),
            title: "low".into(),
            detail: "d".into(),
            location: None,
            node_ids: vec![],
            evidence: None,
            remediation: "r".into(),
            confidence: 1.0,
            impact: Impact {
                score: 100,
                degree: 0,
                affected_count: 0,
                generated: false,
            },
        };
        let high = Finding {
            rule_id: "high".into(),
            severity: Severity::High,
            confidence: 0.5,
            impact: Impact {
                score: 1,
                degree: 0,
                affected_count: 0,
                generated: false,
            },
            ..low.clone()
        };
        let med_a = Finding {
            rule_id: "med-a".into(),
            severity: Severity::Medium,
            confidence: 0.9,
            impact: Impact {
                score: 1,
                degree: 0,
                affected_count: 0,
                generated: false,
            },
            ..low.clone()
        };
        let med_b = Finding {
            rule_id: "med-b".into(),
            severity: Severity::Medium,
            confidence: 0.9,
            impact: Impact {
                score: 5,
                degree: 0,
                affected_count: 0,
                generated: false,
            },
            ..low.clone()
        };
        let r = ReadinessReport::from_findings(vec![low, med_a, med_b, high], vec![]);
        assert_eq!(r.findings[0].rule_id, "high");
        assert_eq!(r.findings[1].rule_id, "med-b");
        assert_eq!(r.findings[2].rule_id, "med-a");
    }

    fn shadows_edge(from: &str, to: &str) -> Edge {
        Edge {
            source: NodeId(from.into()),
            target: NodeId(to.into()),
            relation: "shadows".into(),
            confidence: synaptic_core::Confidence::Extracted,
            source_file: String::new(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: Some("generated".into()),
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn shadows_edge_becomes_resource_shadow_finding() {
        let gen = node(
            "gen",
            "x.json",
            "src/main/generated/assets/mymod/models/x.json",
            1,
            1,
        );
        let src = node(
            "src",
            "x.json",
            "src/main/resources/assets/mymod/models/x.json",
            1,
            1,
        );
        let graph = kg(vec![gen, src], vec![shadows_edge("gen", "src")]);
        let report = audit(&graph, &AuditOptions::default());
        let f = report
            .findings
            .iter()
            .find(|f| f.rule_id == "READY-RESOURCE-SHADOW")
            .expect("shadow finding");
        assert_eq!(f.severity, Severity::Medium);
        assert!(
            f.node_ids.contains(&"gen".to_string()) && f.node_ids.contains(&"src".to_string()),
            "finding links both resources: {:?}",
            f.node_ids
        );
    }
}
