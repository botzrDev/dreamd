#!/usr/bin/env python3
"""Minimal MCP stdio client for the dreamd alpha suite.

Spawns `dreamd mcp --project-root <root>`, performs the required
initialize -> notifications/initialized handshake, then issues each tool call
given as a JSON array on stdin. Prints a JSON array of the tool results.

One process == one simulated harness (the caller sets source_harness per call),
so cross-harness recall is proven by running this twice: one process appends,
a second, independent process searches.

Usage:
    echo '[{"name":"append_node","arguments":{...}}]' | \
        mcp_driver.py <dreamd-bin> <project-root>
"""
import json
import os
import subprocess
import sys


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: mcp_driver.py <dreamd-bin> <project-root>", file=sys.stderr)
        return 2
    bin_path, project_root = sys.argv[1], sys.argv[2]
    calls = json.load(sys.stdin)

    proc = subprocess.Popen(
        [bin_path, "mcp", "--project-root", project_root],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,  # MCP keeps logs off stdout; drop stderr noise
        text=True,
        bufsize=1,
        env=os.environ,
    )

    def send(obj):
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()

    def recv_id(want_id):
        # Read newline-delimited JSON-RPC until the response with `want_id`,
        # skipping any server-initiated notifications (which carry no id).
        while True:
            line = proc.stdout.readline()
            if not line:
                return None
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == want_id:
                return msg

    # Handshake.
    send({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "alpha-suite", "version": "0.0.1"},
        },
    })
    init = recv_id(1)
    if init is None or "result" not in init:
        print(json.dumps({"fatal": "initialize failed", "got": init}))
        proc.kill()
        return 3
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    results = []
    next_id = 2
    for call in calls:
        send({
            "jsonrpc": "2.0", "id": next_id, "method": "tools/call",
            "params": {"name": call["name"], "arguments": call["arguments"]},
        })
        results.append(recv_id(next_id))
        next_id += 1

    try:
        proc.stdin.close()
        proc.wait(timeout=5)
    except Exception:
        proc.kill()

    print(json.dumps(results, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
