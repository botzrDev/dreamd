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

## CI

The **Alpha suite (cross-harness recall)** job in `.github/workflows/ci.yml`
(WEG-423) runs this suite on every push / PR to `main`, so a silent recall
regression (append→index→read broke once while the engine unit tests stayed
green — WEG-264) can't ship unnoticed. The job's exit code is the gate. Repro it
locally with the exact commands CI runs:

```bash
cargo build -p dreamd && scripts/alpha/alpha-suite.sh
```

Only `alpha-suite.sh` is wired into CI; the `quality-suite.sh` LLM-judge suite in
this directory is a separate, manually-run tool.

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

---

# install-funnel suite — the install front door

`install-funnel-suite.sh` (AILAB-555) is the sandboxed end-to-end gate on what a
first-time user actually runs: `dreamd setup`, `dreamd doctor`,
`dreamd update --dry-run`, and `setup` against MCP configs that are already
occupied.

## Run

```bash
cargo build -p dreamd                       # the suite runs the debug binary
bash scripts/alpha/install-funnel-suite.sh  # from repo root
```

Exit `0` and `23 passed, 0 failed` means the funnel holds.

## CI

The **Install funnel suite** job in `.github/workflows/ci.yml` runs it on every
push / PR to `main`. Unlike the alpha job it is **gating** (no
`continue-on-error`): it starts no daemon, polls nothing, and hits no network,
so it has no flake budget to earn. Repro it locally with the exact commands CI
runs:

```bash
cargo build -p dreamd && bash scripts/alpha/install-funnel-suite.sh
```

## What it proves

- **Cold install** — `setup --yes --harness claude` scaffolds `.agent/` and
  writes `.mcp.json` with the **floating** `npx -y dreamd-mcp` pin. Asserted on
  *parsed* JSON, never on the pretty-printed text: key order is not a contract,
  and a hard `dreamd-mcp@x.y.z` arg is the specific thing that must never appear
  (AGENTS.md `npm-dreamd-mcp-unscoped`).
- **`doctor`** exits 0 on the scaffold `setup` just produced. Never
  `--repair` — that suite hangs (AILAB-561).
- **Idempotence** — a second `setup` exits 0 and leaves `.mcp.json`
  byte-identical. "Already wired" means no rewrite, not a rewrite that happens
  to match.
- **`update --dry-run`** exits 0 and prints the restart contract (AILAB-552)
  without touching the cache or reaching the network.
- **Conflict taxonomy** — every row of the ratified AILAB-548 §3/§6 matrix:

  | Fixture | Asserted |
  |---|---|
  | `dreamd-mcp@1.2.3` pin | exit 1 and no write; `--force` → exit 0, floating pin, third-party servers preserved |
  | `command: "dreamd"` (local build) | exit 1, entry left intact |
  | `npx` + `dreamd-mcp` + `--project-root` | exit 0, file **byte-identical** — the row a naive implementation rewrites |
  | malformed JSON | exit 1, and exit 1 **again with `--force`** (§6C), file untouched |
  | `--harness both`, only `.cursor/mcp.json` conflicts | exit 1, `.mcp.json` never created (§6A atomic) |

## Sandbox

`HOME` is redirected to a throwaway `mktemp -d` before any CLI call, so the real
`~/.agent` registry/daemon and `~/.cache/dreamd-mcp` are never touched (the
sandbox is removed on exit). `$DREAMD_SOCK` is unset for the same reason — an
exported override would aim `setup`'s liveness probe and `doctor` at a daemon
outside the sandbox. Every fixture is its own project dir with a `git init`
sentinel. Nothing long-lived is started: no `watch`, no daemon, no network.

## Scope / caveat

This drives the CLI, not the TTY wizard — every invocation passes `--yes`,
because `setup` refuses to prompt off a TTY (AILAB-551, whose own tests cover
the prompts). It also does not test the npm shim or a real `npx` install; it
asserts the config dreamd *writes*, not what npm later resolves.
