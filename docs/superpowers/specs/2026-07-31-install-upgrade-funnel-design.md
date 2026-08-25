# Install / upgrade impress funnel — design

**Date:** 2026-07-31  
**Project:** dreamd-eng (v0.1)  
**Status:** Approved (Approach C)  
**Horizon:** ~1 week to launch impressiveness; dual testing required

## Decisions locked

| Decision | Choice |
|---|---|
| Primary pains | First-run install + upgrade/reinstall (not uninstall-first) |
| Product depth | Full funnel + interactive TTY wizard |
| MCP config | **Write by default** (opt-out `--no-write-mcp`) |
| Testing | Automated `HOME` isolation suite **and** manual clean-box audit |
| Scaffold command | Keep `dreamd init` byte-locked; new `dreamd setup` is the front door |
| Out of scope | Windows native, Homebrew, `dreamd service *`, MCP tool renames, silent clobber of unrelated MCP servers |

## Goals

1. Stranger: empty project → working cross-harness memory with minimal cognition.
2. Returning user: stuck-on-old-binary / live process → `latest` with an obvious restart contract.
3. Proof: CI-runnable isolation suite + written clean-box audit before calling the epic done.

## Product surface

### `dreamd setup` / `npx -y dreamd-mcp setup`

**TTY (stdin is a terminal):** interactive wizard.

1. Resolve project root (same sentinels as init). No auto-`git init`; fail with remediation.
2. Scaffold `.agent/` via existing init internals if needed (do not change init golden stdout path for plain `init`).
3. Ask harness: Claude Code / Cursor / both / skip.
4. Merge floating `dreamd` MCP block (`command: npx`, `args: ["-y","dreamd-mcp"]`) into project `.mcp.json` by default. Preserve other servers. If an existing `dreamd` block is hard-pinned or non-floating, refuse with a clear error unless `--force` (product decision in HITL slice) or dry-run reports the conflict.
5. Offer start shared `dreamd watch` **or** print the exact command (default: print unless `--start-watch` / wizard yes).
6. Verify beat: `doctor` + success card (reload harness → ask agent to `search_nodes`).
7. Success screen: paths written, undo path (`--no-write-mcp` / uninstall docs).

**Non-interactive:**

```text
dreamd setup --yes --harness claude|cursor|both|none
  [--no-write-mcp] [--start-watch|--no-start-watch] [--dry-run] [--force]
```

No prompts. Non-zero on conflict / missing project root.

### Upgrade

- `dreamd update` prints restart contract: stop live `dreamd mcp` / `dreamd watch` if present, cache clear (existing), reload harness / re-run floating npx.
- `dreamd update --restart`: stop local servers (reuse lifecycle_cleanup), then instruct re-launch. No OS service.
- Cheap “running vs package” messaging where available; do not invent a second download path.

### Docs

README / GUIDE / adapter READMEs lead with `npx -y dreamd-mcp setup`. `init` remains documented as the scaffold primitive. Floating-pin rule unchanged (`npx -y dreamd-mcp` only).

## Testing

### Isolation suite — `scripts/alpha/install-funnel-suite.sh`

Pattern after `scripts/alpha/alpha-suite.sh` (throwaway `HOME`):

- Cold `setup --yes --harness claude`
- Assert `.agent/`, merged `.mcp.json` floating pin, `doctor` exit 0
- Idempotent second `setup`
- `update --dry-run`
- Conflict case: pinned dreamd block → non-zero (or dry-run report)
- CI job (or alpha job extension)

Always exercise CLI via `cargo run -p dreamd --bin dreamd` (dual-bin gotcha).

### Manual clean-box audit

Checklist under `context/audits/` (Jul-22 uninstall audit spirit):

- Fresh WSL and/or macOS user
- Claude Code + Cursor timed first-run with wizard
- Upgrade while `watch` still running
- Capture logs; file bugs from failures before epic Done

## Non-goals (repeat)

- Auto-`git init`
- Cursor **global** MCP as default write target (project `.mcp.json` first; global remains documented example only unless HITL expands)
- Uninstall semantics redesign (AILAB-226)
- Windows / brew / systemd LaunchAgent

## Ticket slices

Linear epic: [AILAB-547](https://linear.app/wegetit/issue/AILAB-547)

| # | ID | Type | Title | Blocked by |
|---|-----|------|-------|------------|
| 1 | [AILAB-548](https://linear.app/wegetit/issue/AILAB-548) | HITL | Lock wizard copy + harness write targets + conflict policy | — |
| 2 | [AILAB-549](https://linear.app/wegetit/issue/AILAB-549) | AFK | `dreamd setup` clap + non-interactive scaffold | — |
| 3 | [AILAB-550](https://linear.app/wegetit/issue/AILAB-550) | AFK | MCP merge writer | 548, 549 |
| 4 | [AILAB-551](https://linear.app/wegetit/issue/AILAB-551) | AFK | TTY interactive wizard | 548, 549, 550 |
| 5 | [AILAB-552](https://linear.app/wegetit/issue/AILAB-552) | AFK | `update` restart contract + `--restart` | — (related 226) |
| 6 | [AILAB-555](https://linear.app/wegetit/issue/AILAB-555) | AFK | install-funnel isolation suite + CI | 549, 550, 552 |
| 7 | [AILAB-553](https://linear.app/wegetit/issue/AILAB-553) | AFK | Docs front door → `setup` | 549, 550 |
| 8 | [AILAB-554](https://linear.app/wegetit/issue/AILAB-554) | HITL | Clean-box audit | 551, 552, 555 | 

## Related

- AILAB-226 (`uninstall` / `update` baseline — In Review)
- AILAB-227 / AILAB-497 (uninstall docs order)
- AGENTS.md drift: `npm-dreamd-mcp-unscoped`, `cargo-run-dreamd-needs-bin`
