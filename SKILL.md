# dreamd — Agent Memory Skill

**Works with:** Claude Code, Cursor, Cline, OpenCode, Codex CLI, Gemini CLI, Copilot agent mode, Roo Code, Goose — any MCP-aware harness.

dreamd gives you persistent memory across every coding agent you use. What Claude Code learns in one session, Cursor already knows the next. Memory lives in `.agent/` in your repo, checked into git, readable as plain text.

---

## Protocol note — do this first

After `initialize`, send `notifications/initialized` before calling any tools.  
Without it, `search_nodes` returns empty results silently.

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

Returns a ranked list of episodic events scored by BM25 × salience (recency, pain, importance, recurrence).

Each result carries `metadata.skill_action` (its cluster key) and `metadata.source_harness` (the harness that authored it), so you can see each hit's cluster and which tool taught it.

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
| `client_dedup_key` | string | no       | Idempotency key. First 60 chars of content, lowercased, spaces → underscores. Prevents duplicate writes on retry. |

Returns 201 after `fdatasync`. The write is durable before the response arrives.

**Call `append_node` when:**

- You solved a problem that took more than one attempt
- You discovered a constraint, gotcha, or project convention
- The user says "remember this," "note that," or "log this lesson"
- A build or test failure revealed a non-obvious fix

---

## `source_harness` values

`source_harness` is a free-form string — the server validates presence only (omitting it causes a deserialization error). The values below are the naming convention used by dreamd. Use them consistently so the dream cycle clusters correctly.

| Agent         | Value         | Status   |
| ------------- | ------------- | -------- |
| Claude Code   | `claude-code` | verified |
| Cursor        | `cursor`      | expected |
| Cline         | `cline`       | expected |
| OpenCode      | `opencode`    | expected |
| Codex CLI     | `codex`       | expected |
| Gemini CLI    | `gemini-cli`  | expected |
| Copilot agent | `copilot`     | expected |
| Roo Code      | `roo-code`    | expected |
| Goose         | `goose`       | expected |

"Verified" = tested end-to-end with 20/20 compliant keys. "Expected" = should work per MCP spec; not yet independently validated.

---

## `skill_action` naming rules

Format: `language::domain::specific`  
Charset: `[a-z0-9_]` segments joined by `::` — dots, hyphens, and slashes are rejected.

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

dreamd periodically consolidates `episodic/AGENT_LEARNINGS.jsonl` into `semantic/LESSONS.md`. When a `skill_action` cluster recurs enough (≥ 3 events in a 7- or 30-day window), dreamd writes a lesson to `LESSONS.md`. You can trigger this manually:

```
dreamd dream
```

You can read `LESSONS.md` directly — it is plain UTF-8 markdown. Your edits are respected on the next read.

---

## Folder layout

```
.agent/
  episodic/AGENT_LEARNINGS.jsonl   # append-only event log — written by append_node
  semantic/LESSONS.md              # consolidated lessons — written by dream cycle
  personal/PREFERENCES.md          # user preferences — never leaves your machine
  .dreamd/                         # implementation state — gitignored
```

All files are UTF-8 plaintext. `.agent/` is checked into git. You can `cat`, `grep`, `git diff`, and hand-edit any file. The agent reads your edits.

---

## What dreamd is not

- Not a replacement for `AGENTS.md` or `SKILL.md`. Those are human-authored project rules; `.agent/` is machine-written runtime memory. They work together.
- Not a vector database. v0.1 uses BM25 lexical recall. Semantic/embedding recall is on the roadmap.
- Not a hosted service. v0.1 makes zero network calls. Everything stays on your machine.

---

## Common mistakes

| Mistake                                                 | Fix                                                                                 |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| Skipping `notifications/initialized`                    | Always send it after `initialize`; `search_nodes` silently returns empty without it |
| Using slashes in `skill_action` (`rust/borrow-checker`) | Use `::` (`rust::borrow_checker`) — slashes are rejected                            |
| Omitting `source_harness`                               | Required field; omitting causes a deserialization error                             |
| Writing vague content                                   | One concrete fact per node; vague content scores low on recall                      |
| Not calling `search_nodes` at session start             | You will re-discover things already known; call it first                            |

---

## Install / MCP config

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "npx",
      "args": ["dreamd-mcp@0.1.0-rc.2"]
    }
  }
}
```

Add to `.mcp.json` in your project root (Claude Code) or your harness's equivalent config file.

No Rust required. Node ≥ 18. Prebuilt binaries: linux-x86_64, darwin-x86_64, darwin-aarch64.

Repo: https://github.com/botzrDev/dreamd
