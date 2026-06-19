# SQL Auditing

CodeGraph models SQL as a first-class part of the code graph and audits it for
**performance** and **security** problems. Extraction turns `.sql` files into
table / column / index / view / trigger / procedure / policy / role nodes, links
the application code that runs SQL to the tables it touches, and the
`codegraph sql` command (plus the `audit_sql` / `advise_sql` MCP tools) runs a
rule engine over that graph.

Two ways to use it:

- **Audit** the SQL already in a repo: `codegraph sql audit`.
- **Advise** on a candidate query before it is written: `codegraph sql advise --query "..."`.

## The SQL-aware graph

`codegraph extract` parses `.sql` with the [`sqlparser`](https://crates.io/crates/sqlparser)
crate (multi-dialect) plus a regex recovery pass for the security DDL that
dialect parsers disagree on. T-SQL/SQL Server files (detected by bracket
identifiers, `NVARCHAR`, `GO`, etc.) take a dedicated regex path that recovers
columns, primary/foreign keys (inline and via `ALTER TABLE ADD CONSTRAINT`), and
indexes (`CREATE [NON]CLUSTERED INDEX`) from the bracketed `IF/BEGIN/END` + `GO`
DDL that `sqlparser` cannot parse. It emits:

| Node kind | From |
|---|---|
| `table` / `view` / `function` / `procedure` / `trigger` | `CREATE TABLE/VIEW/...` |
| `column` | each `CREATE TABLE` column (with `data_type`, `nullable`, `pk`) |
| `index` | `CREATE INDEX` (with `unique`) |
| `policy` | Postgres `CREATE POLICY` / SQL Server `CREATE SECURITY POLICY` |
| `role` | `GRANT ... TO <role>` |

| Edge | Meaning |
|---|---|
| `has_column` | table -> column |
| `has_index` / `indexes` | table -> index, index -> indexed column |
| `references` | foreign key (table -> table) |
| `protected_by` | table -> RLS policy |
| `grants` | role -> table (privilege in the edge context) |
| `reads_from` | view/function/procedure -> table |
| `queries` / `writes_to` / `calls_proc` | **application code -> table/procedure** |

Tables also carry `rls_enabled` (defaults `false`, set `true` by
`ALTER TABLE ... ENABLE ROW LEVEL SECURITY`) and `rls_forced` in their metadata.

### Code -> SQL linkage

The cross-language pass scans application code (Python, JS/TS, Go, Rust, Java, C#)
for SQL string literals passed to query APIs, parses them, and links the
enclosing function to the referenced tables with a `queries` (read), `writes_to`
(INSERT/UPDATE/DELETE), or `calls_proc` edge, carrying the query text. This is
what makes "how is this SQL linked into the code" a graph traversal:

```sh
# what code reads or writes the orders table?
codegraph affected orders --relation queries --relation writes_to
```

These edges are `INFERRED` (best-effort string detection), like the other
[cross-language edges](Cross-Language-Edges).

## Querying the SQL layer (CGQL)

Because the SQL layer is first-class, [CGQL](Querying) can query it directly. SQL
objects match by `kind`, and tables expose `rls_enabled` / `dialect`:

```sh
# tables with row-level security disabled
codegraph search 'MATCH (t:table) WHERE t.rls_enabled = "false" RETURN t'

# every column in the schema
codegraph search 'MATCH (c:column) RETURN c'

# policies protecting a table
codegraph search 'MATCH (t:table)-[:protected_by]->(p:policy) RETURN t, p'
```

## The rules

`sql audit` runs every rule and returns findings sorted by severity, each with a
location, the offending object/query, a remediation, a confidence score, and the
graph evidence that triggered it.

### Security

| Rule | Severity | Flags |
|---|---|---|
| `SEC-RLS-001` | High | a table with a tenant/owner column but no row-level security |
| `SEC-RLS-002` | High | RLS enabled but not `FORCE`d (the table owner bypasses it) |
| `SEC-RLS-003` | High | a policy with `USING` but no `WITH CHECK` (writes can leak) |
| `SEC-RLS-004` | Medium | a view over an RLS table without `security_invoker` (runs as owner, bypassing RLS) |
| `SEC-RLS-005` | High | a SQL Server table with sensitive columns and no `CREATE SECURITY POLICY` |
| `SEC-GRANT-001` | Medium | an over-broad grant (`GRANT ALL` / to `PUBLIC`) |
| `SEC-PII-001` | Medium | a column named like a password/secret (confirm it is hashed/encrypted) |
| `SEC-INJ-001` | Critical | a query built by string concatenation/interpolation (injection risk) |

### Performance

| Rule | Severity | Flags |
|---|---|---|
| `PERF-IDX-001` | High | a likely foreign-key column (`*_id`) with no index |
| `PERF-IDX-002` | Medium | an RLS policy filter column with no index (scans every request) |
| `PERF-SEL-001` | Low | `SELECT *` |
| `PERF-SARG-001` | Medium | a non-sargable predicate (function on a column, leading-wildcard `LIKE`) |
| `PERF-DML-001` | High | `UPDATE` / `DELETE` with no `WHERE` |
| `PERF-RAND-001` | Low | `ORDER BY RAND()` / `RANDOM()` |
| `PERF-N1-001` | High | a query executed inside a loop (N+1) |
| `PERF-JOIN-001` | Low | a query with many joins or the same column ORed against several values (use `IN`/`UNION`) |
| `PERF-PLAN-001` | High | a real sequential scan, from a live `EXPLAIN` (see below) |

### Design

| Rule | Severity | Flags |
|---|---|---|
| `DES-PK-001` | Medium | a table with no primary key |
| `DES-FK-001` | Low | a key-typed (`uuid`/`int`) `*_id` column, not the primary key, with no foreign key (`fk_target`) |
| `DES-INS-001` | Low | an `INSERT` with no explicit column list (positional binding breaks silently) |

Many rules are heuristics (tenant/FK/secret detection keys off column-name
conventions; injection and N+1 are best-effort), so each finding carries a
**confidence** in `[0,1]`. They are advisory, not proofs.

## CLI

```sh
# audit the SQL in the current graph
codegraph sql audit                          # writes codegraph-out/sql/{findings.json, audit.md}
codegraph sql audit --severity high          # only high+critical
codegraph sql audit --json                   # machine-readable to stdout
codegraph sql audit --repo backend           # scope to one federated member

# critique a candidate query before writing it
codegraph sql advise --query "SELECT * FROM orders WHERE tenant_id = 1"
```

`sql audit` reads the call-site source (via `--root`, default `.`) so the N+1
rule can see loops; the other rules run purely over the graph.

## MCP tools

The server exposes two read-only tools an assistant can call:

- **`audit_sql`** — audit the whole graph's SQL; optional `severity` filter.
- **`advise_sql`** — critique one `query` (optional `dialect`), cross-referenced
  against the graph's tables, indexes, and RLS. Use it while drafting SQL.

Both return text plus `structuredContent` (the `AuditReport`). See
[MCP Server](MCP-Server).

## Live EXPLAIN (optional)

By default the auditor is fully static and offline. Built with the `live-explain`
feature, `sql audit --explain --db-url <url>` connects to a database and runs
`EXPLAIN` (never `EXPLAIN ANALYZE`, so it does not execute your query) to confirm
which queries really do a sequential scan, raising `PERF-PLAN-001` at high
confidence:

```sh
cargo install --path bin/codegraph --features live-explain
codegraph sql audit --explain --db-url postgresql://user@host/db
```

The plan parser is Postgres-shaped; MySQL/SQLite connect but report fewer signals.

## Dialects and limitations

- Dialect coverage: Postgres and SQL Server get the deep RLS rules; MySQL and
  SQLite have no native row-level security, so those are structural/grant checks
  only. Pass `--dialect` to `sql advise` as a hint; extraction infers per file.
- Code -> SQL linkage detects raw SQL strings in the languages above; ORM/query
  builders and SQL assembled across multiple statements are not yet modeled.
- Graph size: column (and index) nodes dominate a SQL graph. On a column-heavy
  schema, `codegraph extract --no-columns` keeps the table / RLS / policy /
  grant / view facts but drops the per-column detail (and the column-level rules
  that depend on it), shrinking `graph.json`.
- Detection is best-effort and confidence-scored; treat findings as a prioritized
  to-do list, not a gate (though `--severity` + `--json` make a CI gate easy to
  build).
