#!/usr/bin/env python3
"""One-shot MCP tool caller for the kicad-netlist-parser server.

Generic counterpart to the per-tool `test_*.py` scripts: call any tool with
arbitrary arguments without writing a new script. Runs against $NETLIST_MCP_NET if
set, else the in-repo fixture at tests/fixture.net.

    ./mcp_call.py get_net net=/ADCIN1
    ./mcp_call.py filter_components subsystem=/ limit:=5
    ./mcp_call.py audit

`key=value` passes a string; `key:=value` parses the value as JSON (numbers,
booleans, lists). Stdlib only — no pip installs.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


# Netlist under test: $NETLIST_MCP_NET if set, else the in-repo fixture.
NETLIST = Path(os.environ["NETLIST_MCP_NET"]).expanduser() \
    if os.environ.get("NETLIST_MCP_NET") else HERE / "tests" / "fixture.net"
BIN = HERE / "target" / "debug" / "kicad-netlist-parser"
PROTOCOL_VERSION = "2024-11-05"


def parse_args(pairs):
    """Turn `key=value` / `key:=json` tokens into a tool-argument dict."""
    args = {}
    for tok in pairs:
        json_key = tok.split(":=", 1)[0] if ":=" in tok else None
        if json_key is not None and "=" not in json_key:
            args[json_key] = json.loads(tok.split(":=", 1)[1])
        elif "=" in tok:
            key, raw = tok.split("=", 1)
            args[key] = raw
        else:
            sys.exit(f"bad argument {tok!r}: expected key=value or key:=json")
    return args


def main():
    if len(sys.argv) < 2 or sys.argv[1] in ("-h", "--help"):
        sys.exit(__doc__)
    tool = sys.argv[1]
    arguments = parse_args(sys.argv[2:])

    if not NETLIST.exists():
        sys.exit(f"netlist not found: {NETLIST}")
    if not BIN.exists():
        subprocess.run(["cargo", "build"], cwd=HERE, check=True)

    proc = subprocess.Popen(
        [str(BIN), str(NETLIST)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    counter = 0

    def request(method, params):
        nonlocal counter
        counter += 1
        proc.stdin.write((json.dumps({
            "jsonrpc": "2.0", "id": counter, "method": method, "params": params,
        }) + "\n").encode())
        proc.stdin.flush()
        while True:
            raw = proc.stdout.readline()
            if not raw:
                stderr = proc.stderr.read().decode(errors="replace")
                raise RuntimeError(f"server closed stdout answering {method!r}\n{stderr}")
            msg = json.loads(raw.decode())
            if msg.get("id") == counter:
                if "error" in msg:
                    raise RuntimeError(f"{method} failed: {msg['error']}")
                return msg.get("result")

    try:
        request("initialize", {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "mcp-call", "version": "0.1.0"},
        })
        proc.stdin.write(
            (json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized",
                         "params": {}}) + "\n").encode())
        proc.stdin.flush()

        print(f"# {tool}({json.dumps(arguments)}) on {NETLIST}", file=sys.stderr)
        result = request("tools/call", {"name": tool, "arguments": arguments})
        for block in result.get("content", []):
            print(block["text"] if block.get("type") == "text" else json.dumps(block, indent=2))
        if result.get("isError"):
            print("(tool reported isError=true)", file=sys.stderr)
    finally:
        try:
            proc.stdin.close()
        except Exception:
            pass
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    main()
