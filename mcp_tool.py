#!/usr/bin/env python3
"""MCP tool caller for dreamd. Usage: python3 mcp_tool.py <tool_name> <json_args>"""

import json, subprocess, sys


def call_tool(tool: str, args: dict) -> dict:
    proc = subprocess.Popen(
        ["dreamd", "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd="/home/austingreen/Documents/botzr/projects/dreamd",
    )

    init = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "1.0"},
        },
    }
    proc.stdin.write(json.dumps(init).encode() + b"\n")
    proc.stdin.flush()
    proc.stdout.readline()  # consume init response

    req = {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    }
    proc.stdin.write(json.dumps(req).encode() + b"\n")
    proc.stdin.flush()

    line = proc.stdout.readline()
    stderr_out = proc.stderr.read()
    proc.terminate()
    if not line:
        return {"error": stderr_out.decode()[:1000]}
    return json.loads(line.decode())


if __name__ == "__main__":
    tool = sys.argv[1]
    args = json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
    result = call_tool(tool, args)

    # Print structured output
    if "result" in result:
        txt = result["result"]["content"][0]["text"]
        parsed = json.loads(txt)
        print(json.dumps(parsed, indent=2))
    elif "error" in result:
        print(f"Error: {result['error']}", file=sys.stderr)
    else:
        print(json.dumps(result, indent=2))
