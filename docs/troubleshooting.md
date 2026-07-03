# Troubleshooting

Symptom → cause → fix → prevention. For deeper reference see [http-api.md](./http-api.md), [configuration.md](./configuration.md), and [SECURITY.md](../SECURITY.md).

---

## Daemon won't start — address already in use

**Symptom:** `dreamd watch` fails with bind error on `~/.agent/dreamd.sock`.

**Cause:** A previous daemon left a stale socket, or another process holds the path.

**Fix:**

```bash
# Is a dreamd already running?
pgrep -a dreamd

# If not, remove stale socket (same user only)
rm -f ~/.agent/dreamd.sock

# Retry
dreamd watch
```

**Prevention:** Let `dreamd watch` shut down via SIGINT/SIGTERM when possible — v0.1 unlinks the socket on graceful stop. After `kill -9`, stale socket cleanup is manual (or automatic on next `bind_api_socket` recovery).

---

## MCP server won't connect

**Symptom:** Harness shows dreamd MCP disconnected, or stderr says `daemon not found`.

**Cause (common):**

1. `npx dreamd-mcp` not installed / wrong package name (`dreamd-mcp`, not scoped)
2. No `.agent/` in the project — server boots empty backend
3. Daemon expected but not running (Phase 2)
4. Wrong `DREAMD_SOCK` override pointing at a dead path

**Fix:**

```bash
dreamd init                    # creates .agent/ + registry entry
dreamd watch &                 # if you want Phase 2 daemon bridge
npx dreamd-mcp@0.1.0-rc.1      # test stdio server manually
dreamd doctor                  # verify store health
```

Check MCP stderr for `Phase 1 fallback` vs `Phase 2 (Remote backend)`.

**Prevention:** Run `dreamd init` before first MCP session. For multi-agent setups, start `dreamd watch` once per machine.

---

## SIGKILL mid dream cycle — what happens?

**Symptom:** Daemon killed during `dreamd dream` or `POST /api/v1/dream`; store may look inconsistent.

**Cause:** Interrupted dream cycle left `dream_in_progress.wal` and/or temp files.

**Fix:** Restart the daemon or run any command that triggers WAL recovery:

```bash
dreamd watch          # recovers on startup before serving
dreamd doctor         # inspect last_dream_cycle_status
```

Recovery deletes incomplete temp files, removes the WAL, sets `state.json` → `failed`. See [examples/crash-recovery/](../examples/crash-recovery/).

**Prevention:** Don't run overlapping dream cycles (HTTP 409 guards concurrent cycles). Use one daemon writer per machine.

---

## Two agents writing simultaneously

**Symptom:** Torn JSONL lines, interleaved records, or lost appends.

**Cause:** Two **standalone** MCP servers (Phase 1) writing to the same `.agent/` without a shared daemon.

**Fix:**

```bash
# One serialized writer for the machine
dreamd watch
```

Point all harnesses at MCP — they auto-bridge to Phase 2 when the socket is up.

**Prevention:** Never run two `dreamd mcp` / `npx dreamd-mcp` processes against one project without `dreamd watch`. See [GUIDE.md](../GUIDE.md) §6.

---

## How do I reset or clear memory?

| Goal | Command |
|---|---|
| Clear session scratchpad | `dreamd reset workspace` |
| Remove project from daemon registry (keep files) | `dreamd init --uninstall-project` |
| Wipe episodic log | Manually delete/truncate `.agent/episodic/AGENT_LEARNINGS.jsonl`, then re-init the index (or use *Full fresh store* below for a clean slate) |
| Full fresh store | Delete `.agent/` and re-run `dreamd init` |

**Warning:** Deleting `.agent/` is destructive. Commit or back up first if the store has value.

---

## Socket permission denied

**Symptom:** `403 forbidden: peer UID does not match daemon owner` or cannot connect to socket.

**Cause:** Connecting process runs as a different Unix user than the daemon owner. Socket is `0600` — owner only.

**Fix:**

- Start the daemon and MCP client as the **same user**
- Don't use `sudo dreamd watch` unless MCP also runs as root (don't)
- Check `ls -l ~/.agent/dreamd.sock` — should be your user, mode `srw-------`

**Prevention:** Run `dreamd watch` from your normal login session, not a system service account (until v0.1.1 service docs land).

---

## No `.agent/` directory found

**Symptom:** `dreamd: no .agent/ store found` or MCP `coordinator unavailable: no agent root found`.

**Cause:** Project never initialized, or CWD is not inside a project with `.agent/`.

**Fix:**

```bash
cd ~/your-project    # must contain .git/, Cargo.toml, package.json, or pyproject.toml
dreamd init
```

For Cursor global MCP config, pass `--project-root /absolute/path/to/project` (see [adapters/cursor/README.md](../adapters/cursor/README.md)).

**Prevention:** Run `dreamd init` once per repo; commit `.agent/` (except `.agent/.dreamd/` which is gitignored).

---

## Recall returns no results

**Symptom:** `search_nodes` or `GET /api/v1/recall` returns `{"results":[]}`.

**Causes:**

| Cause | Check |
|---|---|
| Empty store | `wc -l .agent/episodic/AGENT_LEARNINGS.jsonl` |
| Read-after-write window | Wait up to 5 s after append (index commit cadence) |
| Query mismatch | Try broader terms from known `content` |
| Wrong project | Verify `X-Agent-Root` / MCP project discovery points at this repo |
| Index stale | `dreamd doctor`; rebuild index if needed |

**Fix:**

```bash
tail .agent/episodic/AGENT_LEARNINGS.jsonl
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $(pwd)" \
  "http://localhost/api/v1/recall?q=known+phrase&k=10"
```

**Prevention:** Use `dreamd watch` for consistent index state; see [GUIDE.md](../GUIDE.md) §3.

---

## Still stuck?

1. `dreamd doctor` — cycle status, config mode
2. [docs/ci.md](./ci.md) — reproduce CI gates locally
3. [GitHub Discussions](https://github.com/botzrDev/dreamd/discussions) for usage questions
4. [SECURITY.md](../SECURITY.md) for vulnerability reports (not public issues)
