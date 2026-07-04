---
name: alpha-test
description: Run and diagnose the dreamd alpha suites at scripts/alpha/ — the cross-harness append→recall plumbing smoke test AND the memory-quality suites (salience-ranking / attribution / dream-cycle golden gate, plus an optional LLM-judge relevance report). Use when the user wants to alpha-test dreamd, run the alpha suite, verify cross-harness recall (claude-code ↔ cursor), test whether recall/memory is any *good* (not just that it happens), or debug why a check fails.
---

# dreamd alpha test

Two automated suites live at `scripts/alpha/`:

1. **Plumbing** (`alpha-suite.sh`) — proves a learning appended by one harness is
   *recalled* by an independent second harness, on both the daemon path (Phase 2)
   and the no-daemon JSONL-replay path (Phase 1). Uses lexically-unique payloads,
   so it proves recall *happens* but says nothing about whether it's good.
2. **Quality** (`quality-suite.sh` + optional `quality_judge.py`) — proves the
   memory is actually *good* through the MCP boundary: salience-weighted ranking,
   attribution, and dream-cycle promotion (deterministic gate), plus a fuzzy
   natural-language relevance report (LLM judge). See **Quality suite** below.

Both are the **code-path** proof only; the real GUI round-trip (actual Cursor /
Claude Code MCP clients + screenshot) is the manual DEMO-4 runbook, out of scope.

## Run

```bash
cargo build -p dreamd            # both suites run target/debug/dreamd
scripts/alpha/alpha-suite.sh     # plumbing — pass = "7 passed, 0 failed"
scripts/alpha/quality-suite.sh   # quality golden gate — pass = "14 passed, 0 failed"
scripts/alpha/quality_judge.py   # optional LLM-judge relevance report (needs an API key)
```

Pass = exit `0` and `7 passed, 0 failed`. Needs `python3` and `git` on PATH.
The suite redirects `HOME=$(mktemp -d)`, so your real `~/.agent` daemon,
registry, and memory are never touched — it kills the daemon and deletes the
sandbox on exit.

## The 7 checks (in order)

- **Phase 2 (daemon up):** (1) daemon binds its socket; (2) a `claude-code`
  append mints an `evt_…` id; (3) a *separate* `cursor` process recalls that
  write (polls ~18s for the daemon's ~5s index-commit cadence).
- **Boundary:** (4) socket is gone after the daemon is killed.
- **Phase 1 (no daemon):** (5) a `cursor` append mints an `evt_…` id; (6) a
  fresh process replays the JSONL and recalls it; (7) that replay **also**
  recalls the earlier Phase-2 write.

## Diagnosing a failure

The sandbox is deleted on exit. To inspect logs, temporarily comment out
`rm -rf "$SANDBOX"` in the `cleanup()` trap, re-run, then read
`$SANDBOX/daemon.log` (the script prints the sandbox path on the first line).

| Failing check | Most likely cause → fix |
|---|---|
| `daemon never bound socket` | Debug binary missing/stale → `cargo build -p dreamd`; or the daemon crashed at startup → read `$SANDBOX/daemon.log`. |
| append `errored` / `no event id minted` | MCP handshake failed (driver emits `{"fatal":"initialize failed"}`), or the payload was rejected. `append_node` **requires** `source_harness` (omitting it = silent deser fail); content >4 KiB is rejected. |
| `phase2 cross-harness recall FAILED` | Index-commit lag exceeded the ~18s poll on a slow box → widen the `seq 1 6` / `sleep 3` loop; **or** a real Phase-2 daemon-bridge regression. Not a stale index — see invariants. |
| `socket still present; not a true Phase 1` | Daemon didn't exit within 3s of SIGTERM → re-run, or widen the shutdown wait loop. |
| `phase1 recall FAILED` | Episodic JSONL read-path broke. Check `AGENT_LEARNINGS.jsonl` for a torn/blank line — `episodic::scan` halts at the first blank or newline-less final line. |
| `phase1 did NOT recall the earlier write` | WEG-378 read-path regression canary: the Phase-2 write must persist in the same JSONL and replay. This check exists specifically to catch that regression. |

## Invariants when reading output

- **Isolation, not reset.** The suite needs no clean-slate step — the `HOME`
  redirection gives a fresh store every run. There is **no** `dreamd reset --all`
  command; never suggest one. (So a stale index can't cause failures here — don't
  chase that ghost; it was the pre-isolation failure mode of the old manual suite.)
- **`isError` substring trap.** An append success payload is
  `{"isError": false, …}`. A bare `grep -i error` false-positives on it — detect
  success by the minted `evt_…` id. Apply this rule if you add assertions.
- **Order is load-bearing.** The daemon must be up *before* the MCP client starts
  (Phase 1 in-process mode is sticky for a client's whole session). The script
  already sequences daemon → client; preserve that order if you edit it.

## Quality suite (is the memory any *good*?)

The plumbing suite can't see a ranking regression — with unique payloads there's
only ever one match. The quality suite closes that gap through the exact MCP
surface a real harness uses. Recall order is `score = bm25 × salience`, where
`salience = exp(−age_days/14) × (pain/10) × (importance/10) × (1 + ln(1+recurrence))`.

**`quality-suite.sh` — deterministic golden gate (`14 passed, 0 failed`):**

- **Ranking (the wedge):** appends a high-pain/importance lesson and a benign one
  that repeats the query terms (so the benign one has *higher* BM25); asserts the
  painful one still ranks #1 and that `salience` (not BM25) drove it. This is the
  headline "recent painful important learnings outrank stale benign ones" claim,
  proven end-to-end — not just in the core lib test
  (`crates/dreamd-core/tests/bm25_fastfield_integration.rs`, which never crosses MCP).
- **Attribution:** asserts a recalled record reports the exact `source_harness` +
  `skill_action` it was appended with (the recall wire DTO carries both).
- **Promotion:** appends ≥3 events sharing one `skill_action` (PROMOTION_THRESHOLD=3),
  runs `SOURCE_DATE_EPOCH=… dreamd dream --no-commit`, asserts the cluster promoted
  into `semantic/LESSONS.md` with a `recurrence_counts.json` count ≥3.

**`quality_judge.py` — LLM-judge relevance report (report-only, never a gate):**
seeds a realistic lessons corpus, issues natural-language queries a dev would
type (no lexical overlap), and asks a model to score whether the top recalled
lesson answers each query. Auth resolves `ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN`
→ an `ant auth login` profile; with none it **skips cleanly and exits 0** (so it
never blocks CI). Model via `DREAMD_JUDGE_MODEL` (default `claude-opus-4-8`). It
prints per-query scores + a mean; a low mean means recall is *lexically* fine but
not *relevant* — a real product signal, not a test failure.

### Diagnosing quality-suite failures

- `RANKING GATE failed` → salience isn't reaching the query-time score. Check that
  `append_node`'s `pain`/`importance` flow to the Tantivy fastfields and that
  `recall` multiplies salience (see `salience.rs`); a fresh regression here would
  also break `bm25_fastfield_integration.rs`.
- `ATTRIBUTION GATE failed` → the provenance fields (`RecallResultJson.metadata`)
  regressed; recent `source_harness`/`skill_action` work is the suspect.
- `PROMOTION GATE` / `RECURRENCE GATE failed` → the dream cycle didn't cluster the
  3 events. Read `$SANDBOX/dream.log`; likely a `skill_action` split or window
  change, or SOURCE_DATE_EPOCH putting the events out of the cluster window.

## Files

- `scripts/alpha/alpha-suite.sh` — plumbing suite (the 7 checks above).
- `scripts/alpha/quality-suite.sh` — quality golden gate (ranking / attribution / promotion).
- `scripts/alpha/quality_check.py` — JSON assertion helper for the golden gate.
- `scripts/alpha/quality_judge.py` — optional LLM-judge relevance report (key-gated).
- `scripts/alpha/mcp_driver.py` — minimal MCP stdio client; one process = one
  simulated harness (`initialize` → `notifications/initialized` → `tools/call`).
- `scripts/alpha/README.md` — the suite's own overview.
