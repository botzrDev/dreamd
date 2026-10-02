# Glossary

Domain terms used across SPEC, ARCHITECTURE, CLI help, and agent skills.

| Term | Definition |
|---|---|
| **dream cycle** | Consolidation pass that turns `episodic/` into `semantic/`. Clusters by `skill_action`, promotes recurring clusters to `LESSONS.md`, prunes stale unpinned events. |
| **salience** | Query-time ranking score: `BM25 × exp(-age_days/14) × (pain/10) × (importance/10) × (1 + ln(1 + recurrence))`. Not stored in the index. See [docs/salience.md](./salience.md). |
| **WAL** | Write-ahead log at `.agent/.dreamd/dream_in_progress.wal`. Records destructive intents before they run; recovery runs on startup if interrupted. |
| **episodic** | Append-only layer: `episodic/AGENT_LEARNINGS.jsonl` — raw timestamped learnings. |
| **semantic** | Distilled layer: `semantic/LESSONS.md` — promoted cluster lessons from the dream cycle. Indexed as documents and scored lexically like everything else; "semantic" here does not mean embeddings. |
| **layer / `source`** | Which of the two a recall hit came from: `episodic` (a raw event) or `semantic` (a lesson, id `lsn_<event id>`). Shown as `source` in recall JSON and `layer` in `dreamd blame` and citations. |
| **pinned** | JSONL flag (`"pinned":true`). Pinned events survive decay pruning. Promotion sets the exemplar event pinned; `dreamd archive --force-unpin` clears it. |
| **recurrence** | Count of events sharing a `skill_action` cluster. Stored in `semantic/recurrence_counts.json`; indexed as a Tantivy fast field. |
| **decay snapshot** | `.agent/.dreamd/snapshots/<date>.jsonl` — where the dream cycle's decay pruner archives stale unpinned events. Not a memory branch. |
| **memory branch / `snap-*`** | A named snapshot of the memory files under `.agent/.dreamd/branches/` (`dreamd memory …`). Each dream cycle also writes an automatic `snap-<UTC timestamp>` ref. See [branching-guide.md](./branching-guide.md). |
| **provenance ledger** | Append-only `.agent/.dreamd/provenance/ledger.jsonl` — derivation edges (event → index document, lesson citation, recurrence). Checked by `dreamd doctor --provenance`. Unsigned; records derivation, not trust. See [provenance.md](./provenance.md). |
| **forget receipt** | The `forget-receipt/1.0` JSON file `dreamd forget --proof <path>` writes: event id, lesson effect, ledger root, orphan edges. Not a signed proof. |
| **skill_action** | Hierarchical clustering key: `[a-z0-9_]` segments joined by `::` (e.g. `rust::error_handling`). Language-first. |
| **agent store** | The `<project>/.agent/` directory — project-scoped memory on disk. |
| **project root** | Directory containing `.agent/` and a repo sentinel (`.git`, `Cargo.toml`, etc.). Sent as `X-Agent-Root` on the HTTP API. |
| **daemon home** | `~/.agent/` — user-scoped `registry.toml`, `dreamd.log`, and `dreamd.sock` while the daemon runs (Windows: `server.json` + `auth.json` instead of the socket). |
| **Phase 1 (MCP)** | In-process MCP server when no daemon is reachable. Safe for single-agent / sequential use. |
| **Phase 2 (MCP)** | MCP bridges to `dreamd watch` over the Unix socket (loopback TCP on Windows) — single serialized writer. |
| **coordinator** | `MemoryCoordinator` actor — sole writer to JSONL and owner of append durability. |
| **registry** | `~/.agent/registry.toml` — maps project root paths to registered stores. |

See also [SPEC.md](../SPEC.md) (contract) and [ARCHITECTURE.md](../ARCHITECTURE.md) (implementation).
