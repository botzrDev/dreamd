# AGENTS.md — dreamd

This file is read by Claude Code, Cursor, Cline, Codex, and other MCP-aware coding
agents when working in this repository. It describes what this project builds, how to
navigate and work in it, and the conventions that govern the codebase.

An agent deciding whether to install dreamd in some other project should read
`README.md` and `SKILL.md` and stop here. This file is the manual for changing
this repository.

The release record is `CHANGELOG.md`. The version is `Cargo.toml`. Ticket history
lives in the tracker, not here.

---

## What this repo builds

dreamd is a local-first, single-binary memory layer for AI coding agents. Claude Code,
Cursor, Cline, and other MCP-aware harnesses read and write to a shared `.agent/` folder
via MCP. What one agent learns, the next already knows. The file system is the source
of truth — plain JSONL and Markdown you can cat, grep, git diff, and hand-edit.

**The one job dreamd is hired for:** memory continuity across tools. Not a second brain,
not an ambient capture product, not a Python framework SDK.

**Trust labels are direction, not shipped.** The provenance ledger records which event
fed which index document and lesson. It is not a trust label: no label, taint, or tier
exists, `source_harness` is caller-asserted, and `docs/provenance.md` is format only.
Do not document a trust label, taint, or "the Memory" as if it shipped. Do not claim
momo `MemoryRegion` objects. Do not describe a larger product, kernel, or sibling
repository that dreamd belongs to in this repository's docs, README, or llms files
(ruled 2026-11-04).

---

## Repository layout

```
crates/dreamd-core/        Core memory engine: Tantivy BM25 index, salience
                           collector, episodic store, dream cycle WAL.
crates/dreamd-cli/         CLI binary. Package name `dreamd`. Two bins:
                           `dreamd` and `generate_man`.
crates/dreamd-protocol/    Shared types: AgentLearning, EventId, SkillAction,
                           RECORD_SCHEMA_VERSION. The HTTP response and
                           recall wire types are in dreamd-core `ingress/`.
packages/dreamd-mcp/       Node.js npx shim (`dreamd-mcp`). Downloads a
                           prebuilt binary — no Rust required at runtime.
adapters/                  Harness configs (.mcp.json.example and friends).
SPEC.md                    On-disk contract for `.agent/`: layout, schema,
                           salience, dream cycle. Implementation-agnostic.
ARCHITECTURE.md            Load-bearing engineering decisions.
CONTRIBUTING.md            Dev setup, PR workflow, DCO sign-off.
SECURITY.md                Threat model and disclosure policy.
CHANGELOG.md               What shipped, by version.
docs/                      Operator and HTTP docs. `docs/windows.md` for Windows.
```

`dreamd --help` is the command list. `ARCHITECTURE.md` is the map of the
coordinator, index, HTTP API, MCP server, and dream cycle. `SPEC.md` is the
on-disk contract. Read those before changing the engine; do not re-derive
them from old tickets.

---

## Build and test

Rust 1.95.0, pinned by `rust-toolchain.toml` and `rust-version` in `Cargo.toml`.

```bash
cargo build
cargo test --workspace
cargo test --all-features --workspace          # CI
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo bench -p dreamd-core
scripts/coverage.sh                            # HTML + lcov
```

`cargo run -p dreamd -- …` runs `dreamd` (`default-run`). `generate_man`
needs `--features manpage --bin generate_man`. CLI unit tests live under `--lib`, not
`--bin dreamd` (`main.rs` is a thin shim; `--bin dreamd` reports 0 tests and
exits 0). (`cargo-run-dreamd-needs-bin`)

All commits require DCO sign-off: `git commit -s`. The user runs commits.
Tagging and publishing stay human steps in `RELEASING.md`.

Stripped `dreamd` must stay at or under 24 MB (25,165,824 bytes, NFR-2) on the
Linux and macOS release builds. The `vectors` feature is measured, never gated,
and stays out of the default binary.

---

## Architectural conventions — read before making changes

**Single-writer, append-only episodic log.**
`episodic/AGENT_LEARNINGS.jsonl` is append-only. Every process that writes a
project's store first takes the project writer lock
(`.agent/.dreamd/writer.lock`, `writer_lock::WriterLock::acquire`). Inside the
holder, appends go through the coordinator actor (`AppendLearning`,
`PrepareCycle`, `CommitCycle`, `Shutdown`). There is no `Mutex<File>` writer.
CLI commands that write (`forget`, `archive`, `memory checkout` and `bisect`,
`doctor --repair`, `setup`, an in-process `dream`) take the lock in-process;
`WriterKind` lists every holder. Do not touch the store without the lock.

Before a write, the holder checks that the locked `writer.lock` is still the
same file. If the store was deleted or replaced under the process, that process
writes nothing more. `dreamd watch` opens the project again on the next
request; in-process `dreamd mcp` stops and returns the error.

**EventId is daemon-minted.**
`AgentLearning.id` is an `evt_`-prefixed Crockford base32 ULID minted by the
coordinator. Any inbound `id` is overwritten, along with `schema_version` and
`timestamp`. Callers pass a valid placeholder `EventId`. Clients never supply
the durable id.

**MCP tool names are locked.**
The MCP server exposes `search_nodes` and `append_node`. These names match the
Anthropic reference memory server intentionally. Do not rename them. There is
no layer filter on `search_nodes`.

**Salience is computed at query time.**
`BM25 × exp(-age_days/14) × (pain/10) × (importance/10) × (1 + ln(1 + d(recurrence)))`,
where `d(0) = 0` and `d(r) = 1.5 - 0.5 / r` (`salience::dampen_recurrence`,
SPEC v0.2). Age, pain, and importance come from Tantivy fastfields.
`recurrence` is the all-time subtree count (`recurrence::RecurrenceWarmer`):
live episodic events whose `skill_action` equals the document's key or extends
it with `::`. The score and `d` are never stored. Do not add an indexed score
field. The schema's `recurrence` and `cited_event_count` fastfields are written
as 0 and read by nothing; do not wire recall to them.

`citations[].recurrence` and `blame --explain --json`'s `recurrence` show `d`.
`results[].metadata.recurrence` and the `recall` and `score` tables show the
count. `salience()` clamps first: `age_secs` floors age at 0, and `clamp_score`
holds pain and importance to [0, 10], with NaN as 0. The `/14` is an e-folding
time, not a half-life.

**The semantic layer is `LESSONS.md`, not embeddings.**
`Layer::Semantic` is the lessons document layer in the same Tantivy index. It
is still BM25 × salience. A lesson inherits pain and importance from its
exemplar event; if the exemplar is missing, skip the document. Its
`timestamp_sec` is when the lesson was written (`LessonsFile.last_updated`),
not the exemplar's timestamp. A missing fastfield scores 0 and the hit
disappears.

**Deletes target a unique field.**
`delete_term` uses `event_id` (one document) or `layer` (a whole-layer
replace). Never `skill_action`: episodic and semantic documents share it, so a
cluster-keyed delete removes the cluster's events. Lesson documents use an
`lsn_` id so a per-event delete does not take them with the exemplar.

**Pins are the decay exemption.**
`pinned` short-circuits decay. A dream cycle pins every record the lesson
cites and does not clear pins. A cited exemplar does not age out. `dreamd
archive --force-unpin <id>` is how a pin comes off.

**JSONL readers skip bad lines; rewrites go through `patch_lines`.**
A `\n`-terminated line that does not parse stays in the log and is skipped.
A torn final line that parses as a record gets its `\n`; any other fragment
is truncated. Do not quarantine lines to a sidecar and do not halt on a blank
mid-file line. Every log rewrite (pin pass, decay, `forget`, `archive
--force-unpin`) goes through `episodic::patch_lines`, which keeps unknown
fields, skipped lines, and a torn tail. Do not rebuild a line with
`serde_json::Value`.

**The index is a function of the log.**
Every episodic add upserts on `event_id`. Open and `IndexerMsg::Resync` cover
the set difference between log ids and indexed ids. A test that seeds the
index writes the log first. The index schema is `index/1.3` and self-heals;
`dreamd migrate` does not rewrite it. The exception: a missing or empty log
beside an index that holds events means the log was lost, not the index.
`dreamd doctor --repair` refuses then (`--allow-empty-log` overrides). Do not
rebuild the index from an empty log unasked.

**Dream cycle is WAL-protected.**
A dream cycle and `dreamd forget` stage every output under
`.agent/.dreamd/stage/` and change nothing live until one redo WAL
(`dream_in_progress.wal`, `format: "redo/1"`) commits them. Taking the writer
lock rolls a pending redo WAL forward and wipes any stage directory. Staging
and commit go through `wal::stage_and_commit`: an error before the commit
point removes the stage and sets `failed` in the same process; an error after
it is retried once. Each operation reads the log and `LESSONS.md` once and
plans every output, ledger edges included, with `plan::plan_cycle` /
`plan_forget`. The model call runs outside the coordinator, between
`PrepareCycle` and `CommitCycle`.

**`unsafe` policy.**
`unsafe_code = "forbid"` at workspace level. Every crate inherits it. Do not
add an override. There is no `unsafe` in the workspace.

**Unix domain socket only.**
The HTTP API binds `~/.agent/dreamd.sock` (or `DREAMD_SOCK`) with `SO_PEERCRED`
UID-match on every request. One socket per user. On Unix, `watch` has no TCP
listener; `--bind` and `--insecure` are usage errors there. Windows `watch` is
loopback TCP plus a bearer; a non-loopback `--bind` needs `--insecure` and
keeps the bearer. See `docs/windows.md`. `dreamd mcp --bind` is a different server: Streamable HTTP
at `/mcp`, behind the non-default `mcp-http` feature, with no bearer. It is
not the `/api/v1` router.

**Home directory.**
`layout::home_dir` is the one resolver. Empty `HOME` is unset. There is no
password-database fallback. `USERPROFILE` is only for Windows.

---

## Schema

Every persisted record carries `schema_version: "1.0.0"`
(`dreamd_protocol::RECORD_SCHEMA_VERSION`). That is the episodic record token.
Daemon `state.json` is `"1.0"`. The index manifest is `"index/1.3"`. Do not
conflate them.

`dreamd migrate --from` / `--to` take the episodic token only. The command
does not bak or rewrite the index. Before changing the record schema, register
a real transform in `dreamd-core::migrate`. The shipped stub is
`dreamd migrate --from 1.0.0 --to 1.0.0`. (`migrate-from-to-is-record-schema`)

`skill_action` is the dream-cycle clustering key. Segments match `[a-z0-9_]`,
joined by `::`, language-first (for example
`rust::error_handling::axum_rejection`), at most 256 bytes. `SkillAction::parse`
trims, lowercases, and turns whitespace into `_` first; dots, hyphens, and
slashes are then rejected.

---

## What is shipped, and what is not

Recall is Tantivy BM25 × salience, including the `LESSONS.md` document layer.
Shipped with that: the dream cycle (deterministic, and opt-in LLM with a
deterministic fallback), forget, the provenance ledger, and `dreamd service`
on Linux, macOS, and Windows.

Deprecated since 1.2.0 and removed in 2.0 (`commands/deprecation.rs`): every
`dreamd memory` subcommand (`branch`, `checkout`, `branches`, `delete`, `diff`,
`bisect`), `dreamd vectors enable` with the `vectors` feature, and
`dreamd dream --auto`. They still ship. Do not extend them.

Still out of scope, and not to be described as shipped:

- vector or embedding recall, and hybrid retrieval. The optional `vectors`
  feature downloads a model and does not rank. There is no fusion code.
- a power-loss proof of the Windows directory flush
- a prebuilt `dreamd-mcp` Windows binary
- Homebrew install
- an automatic dream cycle. `dream_cycle_mode = "auto"` makes `watch` fail;
  cycles run when invoked.

`dreamd forget --proof` writes a receipt. Nothing in the ledger is signed.

---

## Working conventions

**`npm-dreamd-mcp-unscoped`.** The npm package is unscoped `dreamd-mcp`.
User-facing docs and `.mcp.json.example` files use the floating form
`npx -y dreamd-mcp` (and `["-y", "dreamd-mcp"]` in MCP configs). Do not
hard-pin `dreamd-mcp@<version>` in those files. `scripts/check-npx-floating.sh`
enforces it. Pin only in `RELEASING.md`, `CHANGELOG.md`, or `docs/spikes/`.

**`doc-first-append-via-uds-learn`.** Adapter docs teach a durable append as
`POST /api/v1/learn` against a live daemon, with a placeholder id. They do not
teach `echo >>` of `AGENT_LEARNINGS.jsonl`. There is no `dreamd learn` command.
A benchmark that writes the log directly is an import, not the adapter pattern.

**`no-hoisted-stdio-lock-across-tantivy`.** Never hold `stdout.lock()` or
`stderr.lock()` across `TantivyIndexHandle::open`, a commit, or anything that
can block on Tantivy's segment updater while tracing may write stderr. A
hoisted lock there deadlocks the process; in-process tests with a `Vec<u8>`
writer stay green. An arm that still hoists a lock carries an inline
`// lock-ok (AILAB-583):` comment naming why it never opens an index. A new
or changed arm that can open one uses unlocked `stdout()` / `stderr()`.

**Privacy banner.** `DR413_DISCLOSURE` in `privacy.rs` is the first-run banner
for `dreamd init` and the first MCP spawn. `tests/fixtures/init.golden.txt` is
byte-locked to it. When the daemon's network behavior changes, change both in
the same commit. Do not leave "planned for vX.Y.Z" in the banner after that
version has shipped.

**Changelog.** Released sections of `CHANGELOG.md` stay as they were published.
A correction to an old release goes under `[Unreleased]`.

---

## License

Apache-2.0. All contributions require DCO sign-off (`git commit -s`).
