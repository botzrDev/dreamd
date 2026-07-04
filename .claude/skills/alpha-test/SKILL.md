---
name: alpha-test
description: Run and diagnose the dreamd alpha suite — the automated cross-harness append→recall smoke test at scripts/alpha/. Use when the user wants to alpha-test dreamd, run the alpha suite, verify cross-harness recall (claude-code ↔ cursor), or debug why alpha-suite.sh fails a check.
---

# dreamd alpha test

Automated proof that a learning appended by one harness is recalled by an
independent second harness — on both the daemon path (Phase 2) and the no-daemon
JSONL-replay path (Phase 1). This is the **code-path** proof only; the real GUI
round-trip (actual Cursor / Claude Code MCP clients + screenshot) is the manual
DEMO-4 runbook and is out of scope for this skill.

## Run

```bash
cargo build -p dreamd           # the suite runs target/debug/dreamd
scripts/alpha/alpha-suite.sh    # from repo root
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

## Files

- `scripts/alpha/alpha-suite.sh` — the suite (bash; the 7 checks above).
- `scripts/alpha/mcp_driver.py` — minimal MCP stdio client; one process = one
  simulated harness (`initialize` → `notifications/initialized` → `tools/call`).
- `scripts/alpha/README.md` — the suite's own overview.
