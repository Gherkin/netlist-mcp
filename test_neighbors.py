#!/usr/bin/env python3
"""Test client for the kicad-netlist-parser MCP server.

Starts the server with `medium_netlist.net`, does the MCP handshake over
stdio, calls the `neighbors` tool with refdes "U8", and dumps the result
to stdout.

Stdlib only — no pip installs. The MCP stdio transport used by rmcp is
newline-delimited JSON-RPC 2.0.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
NETLIST = HERE / "medium_netlist.net"
BIN = HERE / "target" / "debug" / "kicad-netlist-parser"

PROTOCOL_VERSION = "2024-11-05"


def build_server():
    """Build the debug binary if it is not already present."""
    if BIN.exists():
        return
    print("building server (cargo build)...", file=sys.stderr)
    subprocess.run(["cargo", "build"], cwd=HERE, check=True)


class Server:
    """Minimal newline-delimited JSON-RPC client over a child's stdio."""

    def __init__(self, proc):
        self.proc = proc
        self._id = 0

    def _next_id(self):
        self._id += 1
        return self._id

    def _send(self, obj):
        line = json.dumps(obj) + "\n"
        self.proc.stdin.write(line.encode())
        self.proc.stdin.flush()

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def request(self, method, params=None):
        req_id = self._next_id()
        self._send({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params or {},
        })
        # Read lines until we get the matching response (skip any notifications).
        while True:
            raw = self.proc.stdout.readline()
            if not raw:
                stderr = self.proc.stderr.read().decode(errors="replace")
                raise RuntimeError(
                    f"server closed stdout before answering {method!r}.\n"
                    f"--- server stderr ---\n{stderr}"
                )
            msg = json.loads(raw.decode())
            if msg.get("id") == req_id:
                if "error" in msg:
                    raise RuntimeError(f"{method} failed: {msg['error']}")
                return msg.get("result")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "-r", "--refdes", default="U8",
        help="component reference designator to look up (default: U8)",
    )
    args = parser.parse_args()

    if not NETLIST.exists():
        sys.exit(f"netlist not found: {NETLIST}")
    build_server()

    proc = subprocess.Popen(
        [str(BIN), str(NETLIST)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        server = Server(proc)

        # 1. handshake
        init = server.request("initialize", {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test-neighbors", "version": "0.1.0"},
        })
        server_info = init.get("serverInfo", {})
        print(
            f"connected to {server_info.get('name', '?')} "
            f"v{server_info.get('version', '?')}",
            file=sys.stderr,
        )
        server.notify("notifications/initialized")

        # 2. call the tool
        result = server.request("tools/call", {
            "name": "neighbors",
            "arguments": {"refdes": args.refdes},
        })

        # 3. dump the tool output. Text tools return content blocks.
        print(f"=== neighbors(refdes={args.refdes}) ===")
        for block in result.get("content", []):
            if block.get("type") == "text":
                print(block["text"])
            else:
                print(json.dumps(block, indent=2))
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
