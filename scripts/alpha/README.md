# alpha suite — cross-harness recall smoke test

Automated proof of the demo-critical claim: **a learning appended by one harness
is recalled by an independent second harness**, on both daemon and no-daemon paths.

This reconstructs the manual alpha suite (which used to live in throwaway
scratchpads) as a committed, repeatable script.

## Run

```bash
cargo build -p dreamd            # the suite runs the debug binary
scripts/alpha/alpha-suite.sh     # from repo root
```

Exit `0` and `7 passed, 0 failed` means the round-trip works.

## What it does

- Redirects `HOME` to a throwaway `mktemp -d`, so the real `~/.agent` daemon,
  registry, and memory are never touched (cleaned up on exit).
- Scaffolds a real project (`git init` sentinel + `dreamd init`).
- **Phase 2 (daemon):** `dreamd watch` up → one process appends as
  `source_harness=claude-code`; a second, independent process searches as
  `cursor` and must surface the write (polls for the ~5s index-commit cadence).
- **Phase 1 (no daemon):** daemon stopped → in-process append, fresh process
  replays the JSONL and recalls it — including the earlier Phase-2 write, which
  exercises the `episodic::read_all` path.

`mcp_driver.py` is a minimal MCP stdio client (initialize →
`notifications/initialized` → `tools/call`); one process == one simulated harness.

## Scope / caveat

This proves dreamd's **code path** end-to-end. It does **not** drive the real
Cursor / Claude Code GUI MCP clients — that's the manual DEMO-4 runbook, which
produces the screenshot artifact for design-partner outreach.
