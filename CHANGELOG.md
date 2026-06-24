# Changelog

All notable changes to dreamd are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-rc.2] - 2026-06-24

### Added

- Documentation Phase 0–4: `docs/http-api.md`, `docs/configuration.md`, `docs/ci.md`, `docs/troubleshooting.md`, `docs/glossary.md`, `GUIDE.md`, `docs/marketing.md`, `STORY_IDS.md`, expanded adapter READMEs (Claude Code, Cursor, Cline), three new examples (`crash-recovery`, `pinned-events`, `cross-project`), Mermaid diagrams in `ARCHITECTURE.md` and `SPEC.md`, `doc/dreamd.1` man page, `#![deny(missing_docs)]` on `dreamd-protocol`.

### Changed

- `README.md` restructured for install/quick start; marketing narrative moved to `docs/marketing.md`.
- `SECURITY.md` expanded with lesson-injection, privacy, and input-cap sections (merged from `docs/security.md`).
- `docs/security.md` now redirects to canonical `SECURITY.md`.
- `docs/production_schedule.md` is canonical; `docs/video-schedule.md` redirects (removed Antigravity harness drift).

## [0.1.0-rc.1] - 2026-06-22

### Added

- `dreamd watch` — foreground daemon mode (Unix; SIGINT/SIGTERM-graceful shutdown).
- `dreamd dream` — deterministic dream cycle CLI (`--auto` hidden flag; deterministic-only path always available without network). `--dry` preview is planned for v0.1.1.
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
- Recurrence sidecar at `semantic/recurrence_counts.json`; per-`skill_action` cluster counts drive promotion and the salience formula.
- Privacy disclosure printed on first run in any directory without an existing `.agent/` store.
- `npx dreamd-mcp` Node.js shim for zero-install MCP server distribution.
- Agent output redaction: high-entropy strings matching common token patterns are stripped from `content` before append on every path — the HTTP `POST /api/v1/learn` endpoint and the MCP `append_node` tool, both Phase 1 (in-process) and Phase 2 (daemon bridge).
- Criterion benchmark suite for recall latency (`cargo bench -p dreamd-core`).
- `rmcp 1.7.0` (MCP spec 2025-11-25) added as workspace dependency; consumed by `dreamd mcp` subcommand.
- GitHub Actions CI/CD pipeline: lint, test, cross-platform build, binary size gate (NFR-2), DCO sign-off check.
- Release workflow: cross-platform binary builds published to GitHub Releases on tag push.
- Initial project scaffold (Cargo workspace, `SPEC.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`).
- Per-project coordinator routing: each project's `.agent/` store gets its own isolated writer, so memory from one repo can't be misfiled into another.
- `--project-root` flag on `dreamd mcp`, letting IDE adapters pin the project store explicitly (fixes the agent-root mismatch seen in Cursor/Cline).

### Changed

- `AgentLearning.id` is now `EventId` (daemon-minted `evt_`-prefixed Crockford base32 ULID); clients no longer supply IDs — any inbound `id` is overwritten by the coordinator.
- Topology: `npx dreamd-mcp` auto-detects a running daemon and bridges over UDS (Phase 2); otherwise it runs an in-process server (Phase 1). `dreamd watch` provides the shared serialized writer for multi-agent setups.
- `skill_action` keys are normalized and validated to `[a-z0-9_]` segments joined by `::` (e.g. `rust::borrow_checker`); previously-tolerated `.` and `-` are now rejected.
- `dreamd dream` proxies to the running daemon over the Unix domain socket when one is live (running in-process only when no daemon is reachable), so a manual cycle can't race the daemon's writer.

### Fixed

- Dream cycle no longer orphans the coordinator's append file descriptor after the atomic `LESSONS.md`/JSONL renames; appends written during a cycle could previously be dropped.
- `npx dreamd-mcp <subcommand>` routing corrected.
- `dreamd watch` unlinks its Unix domain socket on `SIGTERM`, so a clean restart no longer trips over a stale socket.
- `dreamd init` refuses to scaffold a phantom `.agent/` store when no project-root sentinel is present, instead of leaving an orphan.
- macOS Unix-socket `sun_path` overflow when `$TMPDIR` is long.

### Security

- `~/.agent/` is created atomically at mode `0700` and `registry.toml` is stamped `0600`, closing the brief world-readable window during directory creation.
- `schema_version` is now server-stamped on the raw `POST /api/v1/learn` path (previously client-trusted).
