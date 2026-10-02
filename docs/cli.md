# CLI reference

`docs/dreamd.1` is the top-level list, generated from the clap definitions (`scripts/generate-man.sh`). The `dreamd-<command>(1)` labels in that file are clap's names for those subcommands. There are no separate man pages.

`dreamd --help` and `dreamd <command> --help` are the flag lists. This page is the map, plus the commands that have no other page: `status`, `score`, `setup`, `reset workspace`, and `version`.

On the npm path the shim does not put `dreamd` on `PATH`. Use `npx -y dreamd-mcp <cmd>`. It does not forward `status`, `recall`, `score`, `archive`, or `migrate`; those need a cargo-installed `dreamd`. See [troubleshooting.md](./troubleshooting.md).

## Commands

| Command | What it does | Detail |
|---|---|---|
| `init` | Scaffold `.agent/` and register the project | [GUIDE.md](../GUIDE.md) §1 |
| `setup` | `init`, plus harness MCP config and next steps | [below](#setup) |
| `watch` | Foreground daemon until SIGINT/SIGTERM | [http-api.md](./http-api.md), [install.md](./install.md) |
| `mcp` | MCP server, stdio by default | [mcp-transports.md](./mcp-transports.md) |
| `dream` | Dream cycle | [GUIDE.md](../GUIDE.md) §4, [architecture.md](./architecture.md) |
| `doctor` | Health checks; `--repair` rebuilds the index; `--provenance` checks the ledger | [provenance.md](./provenance.md), [troubleshooting.md](./troubleshooting.md) |
| `status` | Daemon liveness, project, last cycle, log tail | [below](#status) |
| `service` | OS supervisor for `watch` | [install.md](./install.md) |
| `recall` | Ranked search | [observability.md](./observability.md) |
| `blame` | Same ranking, smaller table, default `-k` 5 | [observability.md](./observability.md) |
| `score` | Top memories by salience, no query | [below](#score) |
| `salience-drift` | Salience histogram and 7-day drift | [observability.md](./observability.md) |
| `memory` | Branch, checkout, diff, bisect | [branching-guide.md](./branching-guide.md) |
| `forget` | Remove one event | [provenance.md](./provenance.md), [GUIDE.md](../GUIDE.md) §8 |
| `archive` | `--force-unpin` so decay can prune | [operator-handbook.md](./operator-handbook.md) |
| `migrate` | Episodic schema migrate; the only path is `1.0.0` to `1.0.0` | [migrate.md](./migrate.md) |
| `reset workspace` | Restore `working/WORKSPACE.md` | [below](#reset-workspace) |
| `vectors enable` | Download the optional model; exit 2 on the default binary | [vectors.md](./vectors.md) |
| `uninstall` | Stop processes, unregister, clear caches | [packages/dreamd-mcp/README.md](../packages/dreamd-mcp/README.md) |
| `update` | Clear the native binary cache | same README |
| `version` | Version, commit, build date, target, schema, vectors | [below](#version) |

`dream --dry` prints the `LESSONS.md` it would write and writes nothing. `dream --no-llm` forces the deterministic lesson and still goes through a live daemon. `dream --no-commit` skips the git commit and also skips the daemon, so do not run it while `dreamd watch` is serving that project ([troubleshooting.md](./troubleshooting.md)). `dream --auto` exits 2.

## status

Prints five lines. Exit 0 when the daemon answers, exit 1 when it does not (the lines are still printed).

```text
daemon: running | not running
socket: <path>
project: <root> (registered | not registered)
last_dream_cycle: <time> (<status>) | never run (<status>)
recent log: (none) | recent log (last 5 lines):
```

With no `.agent/` in the working directory, the project line is `project: no .agent/ store in CWD` and the cycle line is `last_dream_cycle: (no store)`. Off Unix the address label is `server_json`, not `socket`. This is not `dreamd service status`.

## score

`dreamd score [-n N] [--explain]` lists up to `N` documents (default 100) with no query text. Every row is scored as if BM25 were 1, so the printed `bm25` is about 1 and `score` is the salience multiplier. `--explain` adds the same factor block as `dreamd recall --explain`.

## setup

`dreamd setup` scaffolds `.agent/` and writes harness MCP config.

| Flag | Effect |
|---|---|
| `-y`, `--yes` | Accept defaults. Required when stdin is not a tty. |
| `--harness claude\|cursor\|both\|none` | Which MCP config to write. Default `both`: `.mcp.json` and `.cursor/mcp.json`. |
| `--no-write-mcp` | Scaffold only. Same store as `dreamd init`. |
| `--force` | Replace an existing `mcpServers.dreamd` entry that does not run `npx -y dreamd-mcp`. |
| `--start-watch` | Start the daemon in the background after scaffolding. |
| `--no-start-watch` | Print the `dreamd watch` command instead (the default). |
| `--dry-run` | Print the planned actions and write nothing. |

## reset workspace

`dreamd reset workspace` writes `working/WORKSPACE.md` back to the scaffold text. It asks for confirmation. `--yes` skips the prompt and is required when stdin is not a tty. It does not touch the episodic log, lessons, or the daemon home.

## version

`dreamd version` prints:

```text
dreamd <semver>
  commit:  <sha>
  built:   <date>
  target:  <triple>
  schema:  1.0
  vectors: on | off
```

`schema` here is the daemon state schema (`1.0`), not the episodic record schema (`1.0.0`). `vectors` is `off` on the default build. `dreamd --version` and `-V` print one line: `dreamd <semver> (<sha> build:<date> target:<triple> schema:1.0 vectors:on|off)`.
