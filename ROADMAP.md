# dreamd roadmap

dreamd is portable, cross-harness memory for coding agents: a natural-language
`.agent/` folder your tools read from and write to in common. This page is
**direction, not dated commitments** — priorities move with what design partners
hit first. The day-to-day view lives in the issue tracker; this is the shape.

## Shipped — v0.1.0

- The open **`.agent/` standard** — folder layout, JSONL node schema, and
  dream-cycle semantics, specified to RFC-2119 in
  [`SPEC.md`](https://github.com/botzrDev/dreamd/blob/main/SPEC.md).
- **Cross-harness memory** — the same lessons in every coding agent you use,
  proven across **Claude Code and Cursor**, out of one repo-local store.
- **MCP server** (`search_nodes` / `append_node`) plus an **HTTP-over-UDS API**
  for direct integration.
- **Deterministic dream cycle** — clustering and consolidation of raw episodic
  events into promoted lessons, reproducible from the same inputs.
- **Crash-safe durable writes** — atomic append with torn-tail recovery, on
  Linux and macOS.
- Every single-repo feature is **free and Apache-2.0**. If you only ever run out
  of one repo, you never pay.

## Shipped — v0.1.1

- **LLM-assisted dream cycle** as an opt-in alternative to the deterministic
  default (falls back when there is no key, the cost cap trips, or `--no-llm`).
- **Semantic indexing** of `LESSONS.md` as a Tantivy document layer — still
  BM25 × salience, not embeddings.
- **Windows watch + learn** — loopback TCP + bearer token from `auth.json`.
  Dream cycle and index stay Unix-only until atomic writes land (see
  [`docs/windows.md`](https://github.com/botzrDev/dreamd/blob/main/docs/windows.md)).
- **`dreamd service`** on Linux (systemd `--user`), macOS (LaunchAgent), and
  Windows (logon scheduled task): install / start / status / restart / uninstall.

## Shipped — v1.0.0

The stable release of everything merged since v0.1.1. Recall is unchanged:
BM25 × salience.

- **Memory branches** — `dreamd memory branch | checkout | branches | delete |
  diff | bisect` over snapshots stored under `.agent/.dreamd/branches/`; every
  dream cycle also leaves a `snap-*` snapshot
  ([`docs/branching-guide.md`](docs/branching-guide.md)).
- **Provenance ledger** — derivation edges recorded under
  `.agent/.dreamd/provenance/`, checked read-only by `dreamd doctor --provenance`.
  Unsigned; no stored root; records derivation, not trust
  ([`docs/provenance.md`](docs/provenance.md)).
- **`dreamd forget <id>`** — removes one event and the lesson state that names
  it, with `--dry-run` and a `--proof` receipt (`forget-receipt/1.0`, not a
  signed proof).
- **Recall inspection** — `dreamd blame`, `recall --without` /
  `--without-cluster`, `explain=1` citations and repeatable `exclude=` on
  `GET /api/v1/recall`, `dreamd salience-drift` and
  `GET /api/v1/observability/salience`
  ([`docs/observability.md`](docs/observability.md)).
- **Recall latency bench write-up** — the in-RAM Criterion bench plus a
  percentile printer you can run yourself
  ([`docs/benchmarks.md`](docs/benchmarks.md)). Latency only; not a
  recall-quality benchmark.
- **Groundwork that does not change recall** — an optional `vectors` cargo
  feature (off by default, not in release binaries) that only downloads a model,
  and a rank-fusion helper nothing calls yet. `POST /api/v1/migrate` exists as a
  501 stub.

## Next

- **Windows atomic writes** — dream cycle and Tantivy index on native Windows.
- **More harness adapters**, including OpenCode.
- Hardening and hot-fixes from launch feedback.

## On the roadmap — not shipped

- **Vector backend and hybrid retrieval** — lexical × vector scoring, built as
  *private derived state* over the canonical natural-language record (never a
  model-specific record on the wire; see the SPEC design principle).
- **Public recall-quality benchmark** — reproducible quality numbers you can run
  yourself (the latency bench above is shipped; quality is not).
- **Multi-agent dream cycle** and the **linter UX** — surfaces we want design
  partners to shape.
- **Cross-repo memory consolidation and governance** — delivered self-hosted
  (never multi-tenant SaaS); the first features in the paid, self-hosted tier.

## Direction — provenance and trust

Direction, not a dated commitment. The provenance ledger
([`docs/provenance.md`](docs/provenance.md)) is the spine of a chain that
shipped in v1.0.0: recording (BZR-155), `doctor --provenance` (BZR-148), the
forget cascade (BZR-158) and `forget --proof` (BZR-151). What shipped records
derivation only — unsigned, with no trust label. Beyond that chain, the
ledger is the substrate a **trust label** would live on: a scalar on every
fragment saying where it came from and how far it may be trusted, joined on
derivation, so that whatever releases an effect can refuse one built on data
below its label. That rule — *every effect traceable to the data that caused
it; no datum causes an effect above its own trust label* — is recorded as a
design in the dreamOS charter in the momo kernel repository (RFC-011,
accepted 2026-09-26), where dreamd is the Linux-hosted proof of memory a
person owns — *the Memory*. It needs its own
spec before any code and is not on the roadmap list above. Today `source_harness`
is caller-asserted and nothing verifies it.

## Principles that won't change

- The `.agent/` store is an **open standard**; the canonical record is
  **natural-language text**, portable to any model by construction.
- **No multi-tenant SaaS.** Paid features are self-hosted or locally-licensed —
  sovereignty is the bet.
- **Single-repo stays free**, Apache-2.0, forever.
