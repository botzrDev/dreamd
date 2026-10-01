# Audit 4 — End-to-end suites and the npm shim

**Verdict: green on the three shell suites.** Alpha, quality, and install-funnel all exited 0 against the debug `dreamd` built from `bb84721` (version line: `dreamd 1.0.0`, commit `bb84721`, built 2026-10-01, `vectors: off`). The npm shim's only failure is the placeholder sha, which audit 2 already treats as expected until the release workflow fills it.

Date: 2026-10-01. Host: Linux x86_64. Each suite sets `HOME` to its own temp directory and deletes it on exit. Wall clock for all three was about 23 seconds.

```bash
bash scripts/alpha/alpha-suite.sh          # exit 0
bash scripts/alpha/quality-suite.sh        # exit 0
bash scripts/alpha/install-funnel-suite.sh # exit 0
```

The binary those scripts executed was the debug binary produced while running the CLI tests (`CARGO_BIN_EXE_dreamd`), linked at `target/debug/dreamd` for the duration of the run. That link was removed afterward. This was not a separate `cargo build --release`.

## Alpha — cross-harness continuity

`scripts/alpha/alpha-suite.sh`. CI job `alpha` runs this with `continue-on-error: true` and a 180s timeout. Local result: **7 passed, 0 failed.**

| Check | Result |
| --- | --- |
| Daemon bound the sandbox socket | Pass |
| Phase 2 append as `claude-code` accepted | Pass |
| A second process, `cursor`, recalled that write through the daemon (after 2×3s) | Pass |
| Daemon stopped | Pass |
| Phase 1 append as `cursor` accepted with no daemon | Pass |
| Replay surfaced that write | Pass |
| Replay also still returned the earlier claude-code write | Pass |

This is the continuity claim: one harness appends, another harness recalls, on the daemon path and on the JSONL replay path. It does not rank, and it does not run a dream cycle.

## Quality — salience, attribution, promotion

`scripts/alpha/quality-suite.sh`. Not referenced under `.github/`. Local result: **14 passed, 0 failed.**

Axis 1, salience. Two memories for the query `database connection pool`. The high-pain record repeats the query less. The low-pain record repeats it more, so raw BM25 prefers the low-pain record.

| Measurement | Value |
| --- | --- |
| Rank | high-salience at 0, low at 1 |
| Salience | high 1.3714, low 0.0210 |
| Final score | high 0.7501, low 0.0159 |
| BM25 | low 0.7553, high 0.5470 |

The gate requires the high-pain record to rank first even though its BM25 is lower. It did.

Axis 2, attribution. An append with `source_harness=claude-code` and `skill_action=redis::cache_stampede` came back from MCP recall with both fields intact.

Axis 3, promotion. Three events in `rust::lifetime_elision` were appended, then `dreamd dream --no-commit` ran in-process after the daemon was stopped. `LESSONS.md` contained the lifetime theme. `recurrence_counts.json` max count was 3.

`--no-commit` is the write path's skip-the-proxy flag. The suite stops the daemon before the cycle so the in-process cycle owns the JSONL. That matches the single-writer rule. It does not exercise `POST /api/v1/dream`.

This suite is the closest automated check to "salience changes order versus raw lexical rank." It is one hand-built pair of memories. It is not a comparison against stuffing the JSONL into a prompt, and it is not WasTrue or LongMemEval. Those evals are outside this tag.

## Install funnel

`scripts/alpha/install-funnel-suite.sh`. This one is a gating CI job (`install-funnel`, 180s timeout). Local result: **30 passed, 0 failed.**

Covered: cold `dreamd setup --yes --harness claude` scaffolds `.agent/` and writes a floating `npx -y dreamd-mcp` block; `dreamd doctor` on that scaffold exits 0; a second setup leaves `.mcp.json` byte-identical; `dreamd update --dry-run` prints the restart contract and exits 0; an update under one `HOME` spares a live `dreamd watch` under a second `HOME`, and that daemon's own update stops it; pinned MCP config is refused without `--force` and rewritten with `--force` while a third-party server entry stays; a `command=dreamd` local entry is refused and left intact; a compatible `--project-root` block is a no-op; malformed JSON is refused with and without `--force` and left untouched; a conflict across both harness files refuses before either file is created.

## Shim

`node --test` in `packages/dreamd-mcp`: 38 tests, 37 passed, 1 failed.

Failed: `every shim platform has a real sha256` because `PENDING_LINUX` is not 64 hex characters. Route tests that would have sent `service`, `memory`, `forget`, `vectors`, `blame`, and `salience-drift` into `dreamd mcp` all passed, so those subcommands are in `DREAMD_SUBCOMMANDS`.

## What this pass did not run

- The same three suites against a stripped release binary.
- `quality-suite.sh` on a CI runner. Nothing in the workflows invokes it, so a future red quality run will not fail a pull request.
- A vectors-feature binary. `dreamd vectors enable` was not shelled out here; the exit-2 path was covered by the unit test in audit 1.
- macOS and Windows end-to-end.
