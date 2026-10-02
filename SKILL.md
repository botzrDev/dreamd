# dreamd — Agent Memory Skill

**Works with:** Claude Code, Cursor, and Cline have adapter docs in this repo. Any MCP client can call the two tools below.

dreamd gives you persistent memory across every coding agent you use. What Claude Code learns in one session, Cursor already knows the next. Memory lives in `.agent/` in your repo, checked into git, readable as plain text.

---

## Protocol note — do this first

After `initialize`, send `notifications/initialized` before calling any tools, as the MCP lifecycle requires.  
A node you just appended becomes searchable after the next index commit (up to 5 s) — an empty result right after an append is that delay, not a failure.

---

## Two tools

### `search_nodes` — recall from memory

```json
{
  "query": "axum error handling",
  "k": 10
}
```

| Field   | Type   | Required | Default |
| ------- | ------ | -------- | ------- |
| `query` | string | yes      | —       |
| `k`     | number | no       | 5       |

Returns `{"results": [...]}`: raw events and consolidated lessons ranked together by BM25 × salience (recency, pain, importance, recurrence). `query` and `k` are the only arguments — there is no layer filter. Use plain words. A colon is query syntax, and a `skill_action` pasted as the query (for example `rust::error`) fails the search.

Each result carries `source` (`episodic` = a raw event, `semantic` = a lesson from `LESSONS.md`; lessons report `source_harness: "dreamd"`), `metadata.skill_action` (its cluster key) and `metadata.source_harness` (the harness that authored it), so you can see each hit's cluster and which tool taught it.

**Call `search_nodes` when:**

- Starting a task in a project you've worked in before
- The user references a past decision, error, or pattern
- You're about to make a choice that may have a documented prior
- Session start — `search_nodes` with the current task description as the query

---

### `append_node` — write to memory

```json
{
  "content": "Axum requires custom Error types to implement IntoResponse.",
  "source_harness": "claude-code",
  "skill_action": "rust::error_handling::axum_rejection",
  "pain": 7.5,
  "importance": 8.0,
  "client_dedup_key": "axum_requires_custom_error_types_to_implement_intoresponse"
}
```

| Field              | Type   | Required | Notes                                                                                                             |
| ------------------ | ------ | -------- | ----------------------------------------------------------------------------------------------------------------- |
| `content`          | string | **yes**  | The learning. One concrete fact or pattern.                                                                       |
| `source_harness`   | string | **yes**  | Your agent identifier — see table below. Omitting causes a deserialization error.                                 |
| `skill_action`     | string | **yes**  | Clustering key. See naming rules below.                                                                           |
| `pain`             | number | no       | 0–10. How disruptive is it to not know this? Default 5.0.                                                         |
| `importance`       | number | no       | 0–10. How broadly applicable is this? Default 5.0.                                                                |
| `client_dedup_key` | string | no       | Idempotency key (any string; convention: first 60 chars of content, lowercased, spaces → underscores). Prevents duplicate writes on retry. |

Returns an MCP JSON-RPC `CallToolResult` after the coordinator has `sync_data`'d the JSONL line (fdatasync-equivalent on Unix). The result text is `{"id":"evt_…","timestamp":"…","deduplicated":false}` — the server mints the id and timestamp. The HTTP learn path returns 201; the MCP tool does not.

**Call `append_node` when:**

- You solved a problem that took more than one attempt
- You discovered a constraint, gotcha, or project convention
- The user says "remember this," "note that," or "log this lesson"
- A build or test failure revealed a non-obvious fix

---

## `source_harness` values

`source_harness` is a free-form string. The server stores what you send and checks only that the field is present (omitting it causes a deserialization error). Clustering uses `skill_action`, not this field. The names below are the convention. Adapter docs in this repo exist for Claude Code, Cursor, and Cline. Aider has HTTP notes. The other rows are names you may send.

| Agent         | Value         | In this repo |
| ------------- | ------------- | ------------ |
| Claude Code   | `claude-code` | adapter      |
| Cursor        | `cursor`      | adapter      |
| Cline         | `cline`       | adapter      |
| Aider         | `aider`       | HTTP notes   |
| OpenCode      | `opencode`    | name only    |
| Codex CLI     | `codex`       | name only    |
| Gemini CLI    | `gemini-cli`  | name only    |
| Copilot agent | `copilot`     | name only    |
| Roo Code      | `roo-code`    | name only    |
| Goose         | `goose`       | name only    |

---

## `skill_action` naming rules

Format: `language::domain::specific`  
Charset: `[a-z0-9_]` segments joined by `::` — dots, hyphens, and slashes are rejected (`invalid skill_action: …`).

```
rust::error_handling::axum_rejection
rust::async::tokio_select_cancel_safety
typescript::testing::vitest_async_timeout
python::deps::virtualenv_activation
general::git::rebase_conflict_resolution
```

Keep it lowercase, `::` -separated, language-first. The domain segment groups related learnings for the dream cycle. Be consistent — `rust::deps::cargo_workspace` clusters with other `rust::deps::*` entries.

---

## Session pattern

**On session start** (before doing any work on a known project):

```json
search_nodes({ "query": "<current task description>", "k": 10 })
```

Read the results. If relevant learnings exist, apply them before proceeding.

**During the session** — write a node when you learn something worth keeping:

```json
append_node({
  "content":        "cargo test --workspace requires all crates to share the same target dir; set CARGO_TARGET_DIR explicitly in CI.",
  "source_harness": "claude-code",
  "skill_action":   "rust::ci::cargo_target_dir",
  "pain":           6.0,
  "importance":     7.0
})
```

---

## The dream cycle

dreamd consolidates `episodic/AGENT_LEARNINGS.jsonl` into `semantic/LESSONS.md`. Clusters with ≥ 3 events in a 7- or 30-day window are promotion candidates; each cycle writes **one** lesson — the highest-salience exemplar from the top cluster. Trigger it with `npx -y dreamd-mcp dream` (or `dreamd dream` after a cargo install).

You can read `LESSONS.md` directly — it is plain UTF-8 markdown. Hand-edits are visible until the next dream cycle, which replaces the file wholesale. Durable JSONL appends go through `append_node` / the daemon, not hand-edits.

---

## Folder layout

```
.agent/
  episodic/AGENT_LEARNINGS.jsonl   # append-only event log — written by append_node
  semantic/LESSONS.md              # consolidated lessons — written by dream cycle
  personal/PREFERENCES.md          # user preferences — committed with `.agent/` unless you gitignore it
  working/WORKSPACE.md             # scratch file
  .dreamd/                         # implementation state — gitignored
```

All files are UTF-8 plaintext. `.agent/` is checked into git (except `.dreamd/`). You can `cat`, `grep`, and `git diff` the store. Durable appends go through `append_node`; the dream cycle overwrites `LESSONS.md`.

---

## What dreamd is not

- Not a replacement for `AGENTS.md` or `SKILL.md`. Those are human-authored project rules; `.agent/` is machine-written runtime memory. They work together.
- Not a vector database. Recall is BM25 lexical × salience, for events and lessons alike. Vector/embedding recall is not shipped.
- Not a hosted service. Memory stays local by default; an LLM-assisted dream cycle is the only opt-in network path (API key required; `--no-llm` stays offline). `npx` downloads the prebuilt binary from GitHub on first run. Everything in `.agent/` stays on your machine.

---

## Common mistakes

| Mistake                                                 | Fix                                                                                 |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| Skipping `notifications/initialized`                    | Send it after `initialize`, per the MCP lifecycle                                   |
| Searching immediately after `append_node`               | The index commits every 5 s; retry after a few seconds                              |
| Using slashes in `skill_action` (`rust/borrow-checker`) | Use `::` (`rust::borrow_checker`) — slashes are rejected                            |
| Omitting `source_harness`                               | Required field; omitting causes a deserialization error                             |
| No `.agent/` in the project                             | In-process, `search_nodes` returns empty and `append_node` errors; run `setup` first |
| Writing vague content                                   | One concrete fact per node; vague content scores low on recall                      |
| Not calling `search_nodes` at session start             | You will re-discover things already known; call it first                            |

---

## Install / MCP config

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "npx",
      "args": ["-y", "dreamd-mcp"]
    }
  }
}
```

Add to `.mcp.json` in your project root (Claude Code) or your harness's equivalent config file.

`npx -y dreamd-mcp setup` writes this block for you (`.mcp.json` for Claude Code, `.cursor/mcp.json` for Cursor) and scaffolds `.agent/`; the tools need that folder to exist.

No Rust required. Node ≥ 18. Prebuilt binaries: linux-x86_64, darwin-x86_64, darwin-aarch64.

Repo: https://github.com/botzrDev/dreamd
