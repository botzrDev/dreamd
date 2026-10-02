# cross-project

Two sibling projects, each with its **own** `.agent/` store. Memory does not cross project boundaries unless you copy files yourself.

## Layout

```
cross-project/
  project-alpha/
    Cargo.toml              # repo root sentinel
    .agent/                 # Alpha's memory (Rust API lessons)
  project-beta/
    package.json            # repo root sentinel
    .agent/                 # Beta's memory (frontend lessons)
```

## Key point

`dreamd init` registers each project root in `~/.agent/registry.toml`, but recall and append are scoped by `X-Agent-Root` (or MCP project discovery). A `search_nodes` in project-alpha never returns project-beta's learnings.

## Try it

The two fixture directories already contain an `.agent/`, so `dreamd init` there prints `dreamd: already initialized — .agent/ exists. nothing to do.` and does **not** register them. A daemon then answers requests for those roots with `404 {"error":"agent root not registered"}`. The fixtures are for reading (see Compare stores below). To see the routing live, use two fresh directories:

```bash
mkdir -p /tmp/xp/alpha /tmp/xp/beta
cd /tmp/xp/alpha && touch Cargo.toml && dreamd init            # registers alpha
cd /tmp/xp/beta  && echo '{}' > package.json && dreamd init    # registers beta — separate JSONL
```

With `dreamd watch` running from either project, the daemon routes per-project coordinators (WEG-272): a `POST /api/v1/learn` with `X-Agent-Root: /tmp/xp/beta` lands in beta's JSONL only, and a recall with `X-Agent-Root: /tmp/xp/alpha` does not return it. Request shapes are in [`docs/http-api.md`](../../docs/http-api.md).

## Compare stores

```bash
wc -l project-alpha/.agent/episodic/AGENT_LEARNINGS.jsonl
wc -l project-beta/.agent/episodic/AGENT_LEARNINGS.jsonl
grep source_harness project-*/.agent/episodic/AGENT_LEARNINGS.jsonl
```

Each fixture holds one learning. Alpha's has `source_harness` `claude-code`; beta's has `cursor`.
