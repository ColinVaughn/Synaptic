//! Security rules: RLS coverage, grants, plaintext secrets, injection.
use crate::findings::{Category, Finding, Severity};
use crate::graphview::{columns_of, nodes_of_kind, out_targets, policies_of, table_flag};
use crate::rules::{query_snippets, AuditCtx, Rule};
use synaptic_core::NodeKind;

/// Column-name hints that a table is tenant/owner scoped (so RLS is expected).
/// Shared with the advise path so both report the same set.
pub const TENANT_HINTS: &[&str] = &[
    "tenant_id",
    "org_id",
    "organization_id",
    "account_id",
    "owner_id",
    "user_id",
    "company_id",
    "workspace_id",
];

/// Column-name hints for a value that must never be stored in plaintext.
const SECRET_HINTS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "api_key",
    "apikey",
    "token",
    "private_key",
    "ssn",
    "credit_card",
];

pub fn register(rules: &mut Vec<Box<dyn Rule>>) {
    rules.push(Box::new(RlsDisabled));
    rules.push(Box::new(RlsEnabledNotForced));
    rules.push(Box::new(PolicyMissingWithCheck));
    rules.push(Box::new(OverBroadGrant));
    rules.push(Box::new(PlaintextSecretColumn));
    rules.push(Box::new(StringBuiltQuery));
    rules.push(Box::new(SqlServerSensitiveTableNoPolicy));
    rules.push(Box::new(ViewWithoutSecurityInvoker));
}

pub struct RlsDisabled;
pub struct RlsEnabledNotForced;
pub struct PolicyMissingWithCheck;
pub struct OverBroadGrant;
pub struct PlaintextSecretColumn;
pub struct StringBuiltQuery;
pub struct SqlServerSensitiveTableNoPolicy;
pub struct ViewWithoutSecurityInvoker;

impl Rule for ViewWithoutSecurityInvoker {
    fn id(&self) -> &'static str {
        "SEC-RLS-004"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for v in nodes_of_kind(ctx.kg, NodeKind::View) {
            // A security_invoker view runs with the querying user's rights, so
            // the underlying RLS still applies. Without it the view runs as its
            // owner and silently bypasses RLS for every caller.
            if table_flag(v, "security_invoker") {
                continue;
            }
            let Some(t) = out_targets(ctx.kg, &v.id, "reads_from")
                .into_iter()
                .find(|t| table_flag(t, "rls_enabled"))
            else {
                continue;
            };
            out.push(Finding {
                rule_id: self.id().into(),
                severity: Severity::Medium,
                category: Category::Security,
                title: format!(
                    "View `{}` reads RLS-protected `{}` without security_invoker",
                    v.label, t.label
                ),
                detail: "A view over a row-level-security table runs with the view owner's rights by default, bypassing the policy for everyone who queries the view (PG15+ adds security_invoker to fix this).".into(),
                location: v.source_location.as_ref().map(|l| format!("{}:{}", v.source_file, l)),
                node_ids: vec![v.id.0.clone(), t.id.0.clone()],
                snippet: None,
                remediation: format!(
                    "Create the view WITH (security_invoker = true) so `{}`'s RLS applies to callers.",
                    t.label
                ),
                confidence: 0.6,
                evidence: Some("view reads an rls_enabled table; security_invoker not set".into()),
            });
        }
        out
    }
}

/// Whether a column name looks like a secret/PII value (exact hint or `_hint`
/// suffix). Shared by SEC-PII-001 and the SQL Server policy-coverage rule.
fn is_secret_column(name: &str) -> bool {
    let name = name.to_lowercase();
    SECRET_HINTS
        .iter()
        .any(|h| name == *h || name.ends_with(&format!("_{h}")))
}

impl Rule for SqlServerSensitiveTableNoPolicy {
    fn id(&self) -> &'static str {
        "SEC-RLS-005"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for t in nodes_of_kind(ctx.kg, NodeKind::Table) {
            if t.extra.get("dialect").and_then(|v| v.as_str()) != Some("sqlserver") {
                continue;
            }
            let sensitive: Vec<String> = columns_of(ctx.kg, &t.id)
                .iter()
                .filter(|c| is_secret_column(&c.label))
                .map(|c| c.label.clone())
                .collect();
            if sensitive.is_empty() {
                continue;
            }
            // On SQL Server, RLS is a CREATE SECURITY POLICY (engine=sqlserver).
            let covered = policies_of(ctx.kg, &t.id)
                .iter()
                .any(|p| p.extra.get("engine").and_then(|v| v.as_str()) == Some("sqlserver"));
            if covered {
                continue;
            }
            out.push(Finding {
                rule_id: self.id().into(),
                severity: Severity::High,
                category: Category::Security,
                title: format!(
                    "SQL Server table `{}` has sensitive columns but no security policy",
                    t.label
                ),
                detail: "On SQL Server, row access is enforced with CREATE SECURITY POLICY (row-level security). A table with sensitive columns and no security policy leaves row scoping entirely to application code.".into(),
                location: t.source_location.as_ref().map(|l| format!("{}:{}", t.source_file, l)),
                node_ids: vec![t.id.0.clone()],
                snippet: None,
                remediation: format!(
                    "Add a predicate function and CREATE SECURITY POLICY ... ON {} to enforce row access in the database.",
                    t.label
                ),
                confidence: 0.5,
                evidence: Some(format!(
                    "dialect=sqlserver, sensitive columns [{}], no security policy",
                    sensitive.join(", ")
                )),
            });
        }
        out
    }
}

impl Rule for RlsDisabled {
    fn id(&self) -> &'static str {
        "SEC-RLS-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for t in nodes_of_kind(ctx.kg, NodeKind::Table) {
            let has_tenant = columns_of(ctx.kg, &t.id)
                .iter()
                .any(|c| TENANT_HINTS.contains(&c.label.to_lowercase().as_str()));
            if has_tenant && !table_flag(t, "rls_enabled") {
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::High,
                    category: Category::Security,
                    title: format!(
                        "Table `{}` has a tenant column but no row-level security",
                        t.label
                    ),
                    detail: "A multi-tenant table with no RLS relies entirely on the application to filter rows; one missing WHERE leaks across tenants.".into(),
                    location: t.source_location.as_ref().map(|l| format!("{}:{}", t.source_file, l)),
                    node_ids: vec![t.id.0.clone()],
                    snippet: None,
                    remediation: format!("ALTER TABLE {} ENABLE ROW LEVEL SECURITY; then add a tenant-isolation policy.", t.label),
                    confidence: 0.6,
                    evidence: Some("tenant-like column present; rls_enabled is false/absent".into()),
                });
            }
        }
        out
    }
}

impl Rule for RlsEnabledNotForced {
    fn id(&self) -> &'static str {
        "SEC-RLS-002"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for t in nodes_of_kind(ctx.kg, NodeKind::Table) {
            if table_flag(t, "rls_enabled") && !table_flag(t, "rls_forced") {
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::High,
                    category: Category::Security,
                    title: format!("RLS on `{}` is not FORCED (table owner bypasses it)", t.label),
                    detail: "ENABLE ROW LEVEL SECURITY does not apply to the table owner. If the app connects as the owner (common with ORMs), the policies do nothing.".into(),
                    location: t.source_location.as_ref().map(|l| format!("{}:{}", t.source_file, l)),
                    node_ids: vec![t.id.0.clone()],
                    snippet: None,
                    remediation: format!("ALTER TABLE {} FORCE ROW LEVEL SECURITY;", t.label),
                    confidence: 0.7,
                    evidence: Some("rls_enabled true, rls_forced false".into()),
                });
            }
        }
        out
    }
}

impl Rule for PolicyMissingWithCheck {
    fn id(&self) -> &'static str {
        "SEC-RLS-003"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for p in nodes_of_kind(ctx.kg, NodeKind::Policy) {
            let has_using = p.extra.get("using_expr").is_some();
            let has_check = p.extra.get("with_check_expr").is_some();
            if has_using && !has_check {
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::High,
                    category: Category::Security,
                    title: format!("Policy `{}` has USING but no WITH CHECK", p.label),
                    detail: "A policy with only USING restricts reads but not writes; a user can INSERT/UPDATE rows they could never read back, leaking data across tenants on write.".into(),
                    location: p.source_location.as_ref().map(|l| format!("{}:{}", p.source_file, l)),
                    node_ids: vec![p.id.0.clone()],
                    snippet: p.extra.get("using_expr").and_then(|v| v.as_str()).map(str::to_string),
                    remediation: "Add a WITH CHECK clause mirroring the USING predicate so writes are constrained too.".into(),
                    confidence: 0.8,
                    evidence: Some("policy using_expr present, with_check_expr absent".into()),
                });
            }
        }
        out
    }
}

impl Rule for OverBroadGrant {
    fn id(&self) -> &'static str {
        "SEC-GRANT-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for e in ctx.kg.edges().filter(|e| e.relation == "grants") {
            let priv_ = e.context.as_deref().unwrap_or("").to_ascii_uppercase();
            let grantee = ctx
                .kg
                .node(&e.source)
                .map(|n| n.label.to_lowercase())
                .unwrap_or_default();
            if priv_.contains("ALL") || grantee == "public" {
                let target = ctx
                    .kg
                    .node(&e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_default();
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::Medium,
                    category: Category::Security,
                    title: format!("Over-broad grant: {priv_} on `{target}` to `{grantee}`"),
                    detail: "Granting ALL privileges or granting to PUBLIC gives more access than an application role needs and widens the blast radius of a compromise.".into(),
                    location: e.source_location.as_ref().map(|l| format!("{}:{}", e.source_file, l)),
                    node_ids: vec![e.source.0.clone(), e.target.0.clone()],
                    snippet: None,
                    remediation: "Grant only the specific privileges the role needs (e.g. SELECT, INSERT) to a named application role, not ALL/PUBLIC.".into(),
                    confidence: 0.7,
                    evidence: Some(format!("privilege={priv_}, grantee={grantee}")),
                });
            }
        }
        out
    }
}

impl Rule for PlaintextSecretColumn {
    fn id(&self) -> &'static str {
        "SEC-PII-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for c in nodes_of_kind(ctx.kg, NodeKind::Column) {
            let name = c.label.to_lowercase();
            if is_secret_column(&name) {
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::Medium,
                    category: Category::Security,
                    title: format!("Sensitive column `{}` may store a secret in plaintext", c.label),
                    detail: "A column named like a password/secret should hold a hash or an encrypted value, never plaintext. Confirm it is hashed (passwords) or encrypted (tokens/keys).".into(),
                    location: c.source_location.as_ref().map(|l| format!("{}:{}", c.source_file, l)),
                    node_ids: vec![c.id.0.clone()],
                    snippet: None,
                    remediation: "Store a strong password hash (argon2/bcrypt) or encrypt the secret at rest; never store plaintext.".into(),
                    confidence: 0.4,
                    evidence: Some(format!("column name matches a secret hint: {name}")),
                });
            }
        }
        out
    }
}

impl Rule for StringBuiltQuery {
    fn id(&self) -> &'static str {
        "SEC-INJ-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        // Injection detection shares the query-text engine; keep only its finding.
        query_snippets(ctx)
            .into_iter()
            .flat_map(|(s, loc, ids)| crate::rules::performance::evaluate_query_text(&s, loc, ids))
            .filter(|f| f.rule_id == self.id())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::AuditCtx;

    fn graph_from(json: serde_json::Value) -> synaptic_graph::KnowledgeGraph {
        let gd: synaptic_core::GraphData = serde_json::from_value(json).unwrap();
        synaptic_graph::KnowledgeGraph::from_graph_data(gd)
    }
    fn ctx(kg: &synaptic_graph::KnowledgeGraph) -> AuditCtx<'_> {
        AuditCtx { kg, root: None }
    }

    #[test]
    fn flags_tenant_table_without_rls() {
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"},
                {"id":"sql:orders:col:tenant_id","label":"tenant_id","file_type":"code","source_file":"s.sql","kind":"column"}
            ],
            "links": [
                {"source":"sql:orders","target":"sql:orders:col:tenant_id","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}
            ]
        }));
        let f = RlsDisabled.check(&ctx(&kg));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule_id, "SEC-RLS-001");
    }

    #[test]
    fn flags_rls_enabled_but_not_forced() {
        let kg = graph_from(serde_json::json!({
            "nodes": [{"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table","rls_enabled":true,"rls_forced":false}],
            "links": []
        }));
        let f = RlsEnabledNotForced.check(&ctx(&kg));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule_id, "SEC-RLS-002");
    }

    #[test]
    fn flags_policy_without_with_check() {
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"},
                {"id":"sql:orders:policy:p","label":"p","file_type":"code","source_file":"s.sql","kind":"policy","using_expr":"tenant_id = 1"}
            ],
            "links": [{"source":"sql:orders","target":"sql:orders:policy:p","relation":"protected_by","confidence":"EXTRACTED","source_file":"s.sql"}]
        }));
        let f = PolicyMissingWithCheck.check(&ctx(&kg));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule_id, "SEC-RLS-003");
    }

    #[test]
    fn flags_grant_all_and_public() {
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"},
                {"id":"sql:role:public","label":"public","file_type":"code","source_file":"s.sql","kind":"role"}
            ],
            "links": [{"source":"sql:role:public","target":"sql:orders","relation":"grants","confidence":"EXTRACTED","source_file":"s.sql","context":"ALL"}]
        }));
        let f = OverBroadGrant.check(&ctx(&kg));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule_id, "SEC-GRANT-001");
    }

    #[test]
    fn flags_password_column() {
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"sql:users","label":"users","file_type":"code","source_file":"s.sql","kind":"table"},
                {"id":"sql:users:col:password","label":"password","file_type":"code","source_file":"s.sql","kind":"column","data_type":"TEXT"}
            ],
            "links": [{"source":"sql:users","target":"sql:users:col:password","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}]
        }));
        let f = PlaintextSecretColumn.check(&ctx(&kg));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule_id, "SEC-PII-001");
    }

    #[test]
    fn flags_string_built_query() {
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"app.get","label":"get_user()","file_type":"code","source_file":"app.py","kind":"function"},
                {"id":"sql:users","label":"users","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.get","target":"sql:users","relation":"queries","confidence":"INFERRED","source_file":"app.py","source_location":"L3","sql":"SELECT * FROM users WHERE name = ' + name + '"}]
        }));
        let f = StringBuiltQuery.check(&ctx(&kg));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule_id, "SEC-INJ-001");
        assert_eq!(f[0].severity, Severity::Critical);
    }

    #[test]
    fn string_built_query_in_test_code_is_not_flagged() {
        // The query lives in a #[test]/#[cfg(test)] function (marked at
        // extraction via _is_test); it is a fixture, not production SQL, so the
        // query-text rules must skip it rather than report a false injection.
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"app.test","label":"it_builds_sql()","file_type":"code","source_file":"app.py","kind":"function","_is_test":true},
                {"id":"sql:users","label":"users","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.test","target":"sql:users","relation":"queries","confidence":"INFERRED","source_file":"app.py","source_location":"L3","sql":"SELECT * FROM users WHERE name = ' + name + '"}]
        }));
        let f = StringBuiltQuery.check(&ctx(&kg));
        assert!(f.is_empty(), "test-code SQL must not be flagged: {f:?}");
    }

    #[test]
    fn flags_sqlserver_sensitive_table_without_security_policy() {
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"sql:patients","label":"patients","file_type":"code","source_file":"s.sql","kind":"table","dialect":"sqlserver"},
                {"id":"sql:patients:col:ssn","label":"ssn","file_type":"code","source_file":"s.sql","kind":"column"}
            ],
            "links": [{"source":"sql:patients","target":"sql:patients:col:ssn","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}]
        }));
        let f = SqlServerSensitiveTableNoPolicy.check(&ctx(&kg));
        assert!(f.iter().any(|x| x.rule_id == "SEC-RLS-005"), "{f:?}");
    }

    #[test]
    fn sqlserver_table_with_security_policy_is_not_flagged() {
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"sql:patients","label":"patients","file_type":"code","source_file":"s.sql","kind":"table","dialect":"sqlserver"},
                {"id":"sql:patients:col:ssn","label":"ssn","file_type":"code","source_file":"s.sql","kind":"column"},
                {"id":"sql:patients:secpol:p","label":"pat_policy","file_type":"code","source_file":"s.sql","kind":"policy","engine":"sqlserver"}
            ],
            "links": [
                {"source":"sql:patients","target":"sql:patients:col:ssn","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"},
                {"source":"sql:patients","target":"sql:patients:secpol:p","relation":"protected_by","confidence":"EXTRACTED","source_file":"s.sql","context":"security_policy"}
            ]
        }));
        let f = SqlServerSensitiveTableNoPolicy.check(&ctx(&kg));
        assert!(
            f.iter().all(|x| x.rule_id != "SEC-RLS-005"),
            "covered table should not flag: {f:?}"
        );
    }

    fn view_over_rls_graph(security_invoker: bool, table_rls: bool) -> serde_json::Value {
        serde_json::json!({
            "nodes": [
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table","rls_enabled":table_rls},
                {"id":"sql:v_orders","label":"v_orders","file_type":"code","source_file":"s.sql","kind":"view","security_invoker":security_invoker}
            ],
            "links": [{"source":"sql:v_orders","target":"sql:orders","relation":"reads_from","confidence":"EXTRACTED","source_file":"s.sql","context":"from"}]
        })
    }

    #[test]
    fn flags_view_over_rls_table_without_security_invoker() {
        let kg = graph_from(view_over_rls_graph(false, true));
        let f = ViewWithoutSecurityInvoker.check(&ctx(&kg));
        assert!(f.iter().any(|x| x.rule_id == "SEC-RLS-004"), "{f:?}");
    }

    #[test]
    fn view_with_security_invoker_is_not_flagged() {
        let kg = graph_from(view_over_rls_graph(true, true));
        let f = ViewWithoutSecurityInvoker.check(&ctx(&kg));
        assert!(f.iter().all(|x| x.rule_id != "SEC-RLS-004"), "{f:?}");
    }

    #[test]
    fn view_over_non_rls_table_is_not_flagged() {
        let kg = graph_from(view_over_rls_graph(false, false));
        let f = ViewWithoutSecurityInvoker.check(&ctx(&kg));
        assert!(f.iter().all(|x| x.rule_id != "SEC-RLS-004"), "{f:?}");
    }

    #[test]
    fn non_sqlserver_sensitive_table_is_not_flagged_by_rls005() {
        // A generic/Postgres table with a secret column is SEC-PII territory, not
        // SEC-RLS-005 (which is SQL Server security-policy coverage).
        let kg = graph_from(serde_json::json!({
            "nodes": [
                {"id":"sql:users","label":"users","file_type":"code","source_file":"s.sql","kind":"table"},
                {"id":"sql:users:col:ssn","label":"ssn","file_type":"code","source_file":"s.sql","kind":"column"}
            ],
            "links": [{"source":"sql:users","target":"sql:users:col:ssn","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}]
        }));
        let f = SqlServerSensitiveTableNoPolicy.check(&ctx(&kg));
        assert!(f.iter().all(|x| x.rule_id != "SEC-RLS-005"), "{f:?}");
    }
}
