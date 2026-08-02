# Repository-memory release validation

Synaptic-Symbols: MemoryStore, search_memory, record_change_outcome

Validate repository memory from the optimized Synaptic binary before release.
The validation must exercise the MCP handshake, default write gate, opt-in
writer, delayed idempotent retry, changed-content conflict, document-backed
decision retrieval, principal isolation, federated reads, compaction reload,
bundle synchronization, benchmark gates, and memory evidence attached to
single-symbol, file-set, working-tree, and PR-impact responses.

## Required commands

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo build --release -p synaptic --locked
synaptic memory refresh --root .
synaptic memory compact --root .
synaptic memory eval --root . --manifest eval/repository-memory.json \
  --principal release-reviewer \
  --repository-claim https://github.com/ColinVaughn/Synaptic \
  --min-recall-at-5 1 --min-mrr 1
```

The restricted benchmark principal keeps private operator/agent-task memories
from perturbing the repository-scoped release corpus.

Run the final MCP session over stdio against `target/release/synaptic`. A
successful response must cite the ADR or procedure source rather than returning
an ungrounded summary. Start a second session with a restricted
`--memory-principal` and verify that unclaimed repository/private records are
absent. Exercise a multi-file `predict_impact`; its structured result must carry
deduplicated `memory_evidence` and `matched_subjects`.

On Windows, the checked-in harness performs those assertions against a writable
principal and a separate restricted, federated principal:

```powershell
.\scripts\validate-repository-memory-mcp.ps1
```

The harness uses a checksummed export/sync bundle for its temporary peer and
removes that peer after both stdio sessions exit.
