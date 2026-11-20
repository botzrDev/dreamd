# dreamd

Local-first memory for AI coding agents. Every memory is a line in a file in the repo. Lessons cite the events they came from. When a memory is wrong, you can trace it and remove it.

```bash
cargo install dreamd --version ^1.3
dreamd setup
```

`--version ^1.3` skips the `0.0.0` name reservation on crates.io. Rust 1.95.0 or newer.

`dreamd setup` writes `.agent/` and the MCP config for Claude Code, Cursor, or both. The harness then spawns `dreamd mcp` over stdio (`search_nodes`, `append_node`). A shared writer is `dreamd watch`.

The files are plain JSONL and Markdown. `dreamd blame` shows what a recall used. `dreamd forget` removes an event and the lesson state built from it.

npm users can stay on `npx -y dreamd-mcp`. Same binary, downloaded for you.

Source, the audit walkthrough, and the threat model: <https://github.com/botzrDev/dreamd>.
