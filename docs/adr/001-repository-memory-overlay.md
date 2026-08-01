# ADR-001: Keep repository memory as a temporal overlay

Status: Accepted
Synaptic-Symbols: MemoryStore, affected, get_node

Repository history has a different lifecycle from the current structural code
graph: it must survive graph rebuilds, retain superseded observations for audit,
and never make an old summary appear to be a current dependency.

## Decision

Persist source-grounded memories under `.synaptic/memory` and join them to the
current graph through revision-aware symbol and path anchors. Graph tools may
render bounded memory evidence, but memory records do not become graph edges.

Every record must cite a source artifact. Corrections append a new record with a
`supersedes` relation instead of rewriting the prior observation. Compaction is
a verified derived snapshot and never replaces the immutable audit records.
Federation and team bundles preserve identity, source, lifecycle, ownership,
and scope.

## Consequences

Agents can see earlier regressions, rejected approaches, and governing decisions
next to a static impact result. The memory store needs its own lifecycle,
idempotency, principal policy, indexing, evaluation, and synchronization
mechanisms.
