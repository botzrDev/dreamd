# Installing dreamd (instructions for AI agents)

dreamd is a local MCP server (stdio). Memory lives in a `.agent/` folder inside the user's project. Two tools: `search_nodes` and `append_node`.

Requirements: Node.js 18+ and Linux x86_64 or macOS (x86_64 / arm64). The npm package has no native Windows binary; on Windows, tell the user to use WSL2 (see [docs/windows.md](./docs/windows.md)).

## 1. Scaffold the project store

Run in the root of the user's project. The root must contain `.git/`, `Cargo.toml`, `package.json` or `pyproject.toml`; if it has none, run `git init` first.

```bash
npx -y dreamd-mcp setup --yes --harness none
```

`--harness none` creates `.agent/` without writing any other tool's MCP config. It prints a privacy notice and a `doctor` check; both are expected.

## 2. Add the server to your MCP settings

Cline: add this entry under `mcpServers` in your Cline MCP settings file (`cline_mcp_settings.json`):

```json
"dreamd": {
  "command": "npx",
  "args": ["-y", "dreamd-mcp", "--project-root", "/absolute/path/to/the/project"],
  "disabled": false,
  "autoApprove": []
}
```

Replace the path with the absolute path of the project from step 1. Cline's MCP settings are global, so `--project-root` tells dreamd which `.agent/` folder to use.

Other harnesses: `npx -y dreamd-mcp setup --harness claude|cursor|both` writes the config for Claude Code and Cursor itself.

## 3. Verify

Call `append_node` with `content: "dreamd install check"`, `source_harness: "cline"`, `skill_action: "setup::verify"`. It returns an `evt_` id, and `.agent/episodic/AGENT_LEARNINGS.jsonl` has the line at once (the memory is plain text).

Then call `search_nodes` with `query: "install check"`. A new event can take about 10 seconds to reach the search index, so if the results are empty, wait 15 seconds and search again before treating it as a failure.

More detail: [adapters/cline/README.md](./adapters/cline/README.md).
