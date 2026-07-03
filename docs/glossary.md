# Glossary

Domain terms used across SPEC, ARCHITECTURE, CLI help, and agent skills.

| Term | Definition |
|---|---|
| **dream cycle** | Consolidation pass that turns `episodic/` into `semantic/`. Clusters by `skill_action`, promotes recurring clusters to `LESSONS.md`, prunes stale unpinned events. |
| **salience** | Query-time ranking score: `BM25 × exp(-age_days/14) × (pain/10) × (importance/10) × (1 + ln(1 + recurrence))`. Not stored in the index. |
| **WAL** | Write-ahead log at `.agent/.dreamd/dream_in_progress.wal`. Records destructive intents before they run; recovery runs on startup if interrupted. |
| **episodic** | Append-only layer: `episodic/AGENT_LEARNINGS.jsonl` — raw timestamped learnings. |
| **semantic** | Distilled layer: `semantic/LESSONS.md` — promoted cluster lessons from the dream cycle. |
| **pinned** | JSONL flag (`pinned: true`). Pinned events survive decay pruning. Promotion sets the exemplar event pinned. |
| **recurrence** | Count of events sharing a `skill_action` cluster. Stored in `semantic/recurrence_counts.json`; indexed as a Tantivy fast field. |
| **skill_action** | Hierarchical clustering key: `[a-z0-9_]` segments joined by `::` (e.g. `rust::error_handling`). Language-first. |
| **agent store** | The `<project>/.agent/` directory — project-scoped memory on disk. |
| **project root** | Directory containing `.agent/` and a repo sentinel (`.git`, `Cargo.toml`, etc.). Sent as `X-Agent-Root` on the HTTP API. |
| **daemon home** | `~/.agent/` — user-scoped `registry.toml` and `dreamd.sock` while the daemon runs. |
| **Phase 1 (MCP)** | In-process MCP server when no daemon is reachable. Safe for single-agent / sequential use. |
| **Phase 2 (MCP)** | MCP bridges to `dreamd watch` over the Unix socket — single serialized writer. |
| **coordinator** | `MemoryCoordinator` actor — sole writer to JSONL and owner of append durability. |
| **registry** | `~/.agent/registry.toml` — maps project root paths to registered stores. |

See also [SPEC.md](../SPEC.md) (contract) and [ARCHITECTURE.md](../ARCHITECTURE.md) (implementation).
