"""Dependency-free, line-framed MCP stdio example. Requires Python 3."""

import json
import sys


def reply(request):
    method = request.get("method")
    if method == "initialize":
        return {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "medha-hello", "version": "0.1.0"},
        }
    if method == "ping":
        return {}
    if method == "tools/list":
        return {
            "tools": [
                {
                    "name": "hello",
                    "description": "Return a greeting for a supplied name.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}},
                        "required": ["name"],
                    },
                }
            ]
        }
    if method == "tools/call":
        name = request.get("params", {}).get("arguments", {}).get("name", "friend")
        return {"content": [{"type": "text", "text": f"Hello, {name}!"}]}
    return None


for line in sys.stdin:
    try:
        request = json.loads(line)
        if "id" not in request:
            continue
        result = reply(request)
        if result is None:
            message = {"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": "Method not found"}}
        else:
            message = {"jsonrpc": "2.0", "id": request["id"], "result": result}
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()
    except (ValueError, TypeError, BrokenPipeError):
        break
