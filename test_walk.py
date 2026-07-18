#!/usr/bin/env python3
"""Test client for the `walk` tool of the kicad-netlist-parser MCP server.

Starts the server with `medium_netlist.net`, does the MCP handshake over
stdio, calls the `walk` tool with a start (pin "REFDES:PIN" or net name), and
pretty-prints a summary of the parsed result.

Stdlib only. The MCP stdio transport used by rmcp is newline-delimited
JSON-RPC 2.0.
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
    if BIN.exists():
        return
    print("building server (cargo build)...", file=sys.stderr)
    subprocess.run(["cargo", "build"], cwd=HERE, check=True)


class Server:
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


def call_walk(server, args_dict):
    result = server.request("tools/call", {
        "name": "walk",
        "arguments": args_dict,
    })
    text = ""
    for block in result.get("content", []):
        if block.get("type") == "text":
            text += block["text"]
    return text, result.get("isError", False)


def summarize(text):
    try:
        obj = json.loads(text)
    except json.JSONDecodeError:
        print("  (non-JSON result):", text)
        return
    print(f"  start={obj['start']!r} start_net={obj['start_net']!r} "
          f"truncated={obj['truncated']}")
    eps = obj.get("endpoints", [])
    print(f"  endpoints: {len(eps)}")
    for e in eps[:12]:
        via = "->".join(v["refdes"] for v in e.get("via", [])) or "(direct)"
        print(f"    d{e['distance']} {e['pin']:<12} {e.get('pin_type')!s:<14}"
              f" {e['component']['value']:<18} via[{via}]")
    if len(eps) > 12:
        print(f"    ... and {len(eps) - 12} more")
    rr = obj.get("rails_reached", [])
    print(f"  rails_reached: {len(rr)}")
    for r in rr[:10]:
        via = "->".join(v["refdes"] for v in r.get("via", [])) or "(direct)"
        print(f"    {r['net']:<20} score={r['score']} via[{via}]")
    ln = obj.get("large_nets", [])
    print(f"  large_nets: {len(ln)}")
    for n in ln[:10]:
        via = "->".join(v["refdes"] for v in n.get("via", [])) or "(direct)"
        print(f"    {n['net']:<20} fanout={n['fanout']} via[{via}]")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("starts", nargs="*",
                        help="one or more start points (pin or net)")
    parser.add_argument("--max-depth", type=int, default=None)
    parser.add_argument("--max-endpoints", type=int, default=None)
    parser.add_argument("--stop-at-power", type=lambda s: s.lower() != "false",
                        default=None)
    parser.add_argument("--raw", action="store_true",
                        help="print raw JSON for the first start")
    args = parser.parse_args()

    starts = args.starts or ["U8:5"]

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
        init = server.request("initialize", {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test-walk", "version": "0.1.0"},
        })
        server_info = init.get("serverInfo", {})
        print(f"connected to {server_info.get('name', '?')} "
              f"v{server_info.get('version', '?')}", file=sys.stderr)
        server.notify("notifications/initialized")

        for i, start in enumerate(starts):
            call_args = {"start": start}
            if args.max_depth is not None:
                call_args["max_depth"] = args.max_depth
            if args.max_endpoints is not None:
                call_args["max_endpoints"] = args.max_endpoints
            if args.stop_at_power is not None:
                call_args["stop_at_power"] = args.stop_at_power
            print(f"\n=== walk({call_args}) ===")
            text, is_err = call_walk(server, call_args)
            if is_err:
                print("  (isError=true)", file=sys.stderr)
            if args.raw and i == 0:
                print(text)
            else:
                summarize(text)
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
