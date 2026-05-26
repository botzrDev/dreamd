# Changelog

All notable changes to dreamd are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `dreamd watch` — foreground daemon mode (Unix; SIGINT/SIGTERM-graceful shutdown).
- `dreamd dream` — deterministic dream cycle CLI (`--dry` preview; `--auto` hidden flag; deterministic-only path always available without network).
- `dreamd reset workspace` — re-scaffolds `working/WORKSPACE.md` to its initial state.
- `dreamd version` — structured version output block with build metadata.
- `dreamd doctor` — health-check output (dream-cycle mode and state-surface diagnostics).
- `dreamd init --uninstall-project` — removes the current project entry from the global registry.
- `--quiet` flag on `dreamd init` to suppress non-essential output.
- HTTP API on Unix domain socket: `POST /api/v1/learn`, `GET /api/v1/recall`, `POST /api/v1/dream`, `GET /api/v1/preferences`.
- MCP server (`dreamd mcp`) — `search_nodes` and `append_node` tools; Phase 1 in-process / Phase 2 UDS bridge.
- `SO_PEERCRED` peer-credential middleware on the UDS API (UID-match enforcement on Linux/macOS).
- `X-Agent-Root` header routing and per-user project registry at `~/.agent/registry.toml`; `dreamd init` registers each project automatically.
- Tantivy 0.26 full-text index with 5-second commit cadence; salience-aware recall collector (BM25 × exponential-decay × pain × importance × log-scaled recurrence).
- Episodic decay: events with score < 2.0 and age > 90 days are archived to `.dreamd/snapshots/YYYY-MM-DD.jsonl`, never deleted.
- Dream-cycle WAL at `.dreamd/dream_in_progress.wal`; startup recovers torn cycles before serving traffic.
- Recurrence sidecar at `.agent/.dreamd/recurrence_counts.json`; per-`skill_action` cluster counts drive promotion and the salience formula.
- Privacy disclosure printed on first run in any directory without an existing `.agent/` store (DR-413).
- `npx dreamd-mcp` Node.js shim for zero-install MCP server distribution.
- Agent output redaction: high-entropy strings matching common token patterns are stripped from `content` before append.
- Criterion benchmark suite for recall latency (`cargo bench -p dreamd-core`).
- `rmcp 1.7.0` (MCP spec 2025-11-25) added as workspace dependency; consumed by `dreamd mcp` subcommand (WEG-77).
- GitHub Actions CI/CD pipeline: lint, test, cross-platform build, binary size gate (NFR-2), DCO sign-off check.
- Release workflow: cross-platform binary builds published to GitHub Releases on tag push.
- Initial project scaffold (Cargo workspace, `SPEC.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`).

### Changed

- `AgentLearning.id` is now `EventId` (daemon-minted `evt_`-prefixed Crockford base32 ULID); clients no longer supply IDs — any inbound `id` is overwritten by the coordinator.
- Topology: `npx dreamd-mcp` is the distribution binary — no separate daemon process required. `dreamd watch` provides an optional persistent-lifetime mode.
