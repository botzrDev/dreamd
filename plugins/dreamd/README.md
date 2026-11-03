# dreamd for Claude Code

dreamd is long-term memory for AI agents that you can trace, review and roll back. Claude records what happened through two MCP tools, `search_nodes` and `append_node`, and dreamd turns recurring patterns into lessons. Every lesson cites the events it came from, and memory lives in `.agent/` in your project as plain files you commit with your code. When an agent acts on something wrong, `dreamd blame` shows which memories drove it and `dreamd forget` removes the bad one with a receipt.

## Setup

In each project, run `npx -y dreamd-mcp init` once and commit the `.agent/` folder. If the project already has dreamd in its `.mcp.json` from `dreamd setup`, remove that entry so you don't run it twice.

## What it runs

The plugin starts the `dreamd-mcp` npm package, which downloads the dreamd binary from this project's GitHub releases and checks its SHA-256. Recall and append make no network calls. The plugin never runs a dream cycle; if you run one yourself with an API key set, it sends event text to Anthropic or OpenAI to write the lesson. Details: https://github.com/botzrDev/dreamd/blob/main/SECURITY.md

Runs on Linux x86_64 and macOS with Node 18 or later. On Windows, use WSL2.
