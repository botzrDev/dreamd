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

`~/.agent/registry.toml` lists both project roots, but recall and append are scoped by `X-Agent-Root` (or MCP project discovery). A `search_nodes` in project-alpha never returns project-beta's learnings.

## Try it

```bash
cd project-alpha
dreamd init    # registers alpha only (if not using fixtures)

cd ../project-beta
dreamd init    # registers beta — separate JSONL
```

With `dreamd watch` running from either project, the daemon routes per-project coordinators (WEG-272) — appends to beta do not land in alpha's JSONL.

## Compare stores

```bash
wc -l project-alpha/.agent/episodic/AGENT_LEARNINGS.jsonl
wc -l project-beta/.agent/episodic/AGENT_LEARNINGS.jsonl
grep source_harness project-*/.agent/episodic/AGENT_LEARNINGS.jsonl
```

Alpha's fixture uses `claude-code`; beta's uses `cursor` — same harness names, different files.
