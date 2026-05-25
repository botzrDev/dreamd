#!/usr/bin/env python3
"""Test MCP connectivity by sending JSON-RPC over stdio to dreamd mcp."""

import json
import subprocess
import sys


def main():
    proc = subprocess.Popen(
        ["dreamd", "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd="/home/austingreen/Documents/botzr/projects/dreamd",
    )

    # Send initialize request
    init_req = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "1.0"},
        },
    }

    msg = json.dumps(init_req) + "\n"
    proc.stdin.write(msg.encode())
    proc.stdin.flush()

    # Read response line
    line = proc.stdout.readline()
    if not line:
        stderr = proc.stderr.read().decode()
        print(f"MCP ERROR (init): {stderr[:500]}", file=sys.stderr)
        return 1

    resp = json.loads(line.decode())
    print(f"Initialize response OK: {json.dumps(resp, indent=2)[:200]}")

    # Send tools/list
    list_req = {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {},
    }
    proc.stdin.write(json.dumps(list_req).encode() + b"\n")
    proc.stdin.flush()

    line = proc.stdout.readline()
    if not line:
        stderr = proc.stderr.read().decode()
        print(f"MCP ERROR (tools/list): {stderr[:500]}", file=sys.stderr)
        return 1

    resp = json.loads(line.decode())
    print(f"tools/list response: {json.dumps(resp, indent=2)[:500]}")

    # Send tools/call for search_nodes
    search_req = {
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "search_nodes",
            "arguments": {"query": "test", "k": 3},
        },
    }
    proc.stdin.write(json.dumps(search_req).encode() + b"\n")
    proc.stdin.flush()

    line = proc.stdout.readline()
    if not line:
        stderr = proc.stderr.read().decode()
        print(f"MCP ERROR (search_nodes): {stderr[:500]}", file=sys.stderr)
        return 1

    resp = json.loads(line.decode())
    print(f"search_nodes response: {json.dumps(resp, indent=2)[:800]}")

    proc.terminate()
    return 0


if __name__ == "__main__":
    sys.exit(main())
