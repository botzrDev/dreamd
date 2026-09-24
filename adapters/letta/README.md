# dreamd — Letta adapter

> **Status:** Code seam only. There is nothing to install yet.

`LettaStore` (`crates/dreamd-core/src/letta.rs`) is a `MemoryStore` delegator. It wraps another `MemoryStore` (the in-process store or the daemon proxy) and forwards `recall_json` and `append` to it unchanged. It adds no JSON shape, rewrites no paths, and makes no network calls.

dreamd's episodic JSONL (`.agent/episodic/AGENT_LEARNINGS.jsonl`) stays the source of truth. Appends still go through the coordinator.

## Not implemented

- Letta MemFS read/write. dreamd does not read from or write to a Letta memory filesystem, and this adapter does not define a file layout for one.
- Conflict handling between dreamd and Letta.
- Install steps. There is no Letta-side configuration to copy.

## MCP

The MCP server does not construct `LettaStore`. The tools are still `search_nodes` and `append_node`. A Letta agent that speaks MCP can use them today through the same `npx -y dreamd-mcp` config as the other adapters.
