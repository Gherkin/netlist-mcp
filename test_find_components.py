#!/usr/bin/env python3
"""Test client for the kicad-netlist-parser MCP server's find_components tool.

Starts the server with `medium_netlist.net`, does the MCP handshake over
stdio, calls `find_components` with the cases from the design doc, and dumps
each result to stdout.

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


def call_find(server, args):
    """Call find_components and return the parsed envelope (or raw text)."""
    result = server.request("tools/call", {
        "name": "find_components",
        "arguments": args,
    })
    texts = [b["text"] for b in result.get("content", []) if b.get("type") == "text"]
    raw = "\n".join(texts)
    print(f"=== find_components({json.dumps(args)}) ===")
    try:
        env = json.loads(raw)
        print(f"query={env['query']!r} returned={env['returned']}")
        for c in env["candidates"][:10]:
            print(f"  {c['refdes']:<6} conf={c['confidence']:.2f} "
                  f"value={c['value']!r} sheet={c['sheet']!r} "
                  f"reason={c['match_reason']!r}")
        if len(env["candidates"]) > 10:
            print(f"  ... ({len(env['candidates']) - 10} more)")
        return env
    except json.JSONDecodeError:
        print(raw)
        return None
    finally:
        if result.get("isError"):
            print("(tool reported isError=true)", file=sys.stderr)


def check(label, ok):
    print(f"  [{'PASS' if ok else 'FAIL'}] {label}")
    return ok


def sorted_desc(env):
    confs = [c["confidence"] for c in env["candidates"]]
    return all(confs[i] >= confs[i + 1] for i in range(len(confs) - 1))


def top(env):
    return env["candidates"][0] if env["candidates"] else None


def main():
    argparse.ArgumentParser(description=__doc__).parse_args()

    if not NETLIST.exists():
        sys.exit(f"netlist not found: {NETLIST}")
    build_server()

    proc = subprocess.Popen(
        [str(BIN), str(NETLIST)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    all_ok = True
    try:
        server = Server(proc)

        # 1. handshake
        init = server.request("initialize", {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test-find-components", "version": "0.1.0"},
        })
        server_info = init.get("serverInfo", {})
        print(
            f"connected to {server_info.get('name', '?')} "
            f"v{server_info.get('version', '?')}",
            file=sys.stderr,
        )
        server.notify("notifications/initialized")

        # 2. cases from find_components_instructions.md "Verify"
        print("\n--- exact ---")
        env = call_find(server, {"query": "TLA2518IRTER"})
        t = top(env)
        all_ok &= check("U8 top at ~1.0, reason exact",
                        t and t["refdes"] == "U8" and t["confidence"] >= 0.99
                        and t["match_reason"].startswith("exact"))

        print("\n--- reverse-join (partial MPN) ---")
        env = call_find(server, {"query": "TLA2518"})
        t = top(env)
        all_ok &= check("U8 top, high base-match",
                        t and t["refdes"] == "U8" and t["confidence"] >= 0.80
                        and "base-match" in t["match_reason"])

        print("\n--- reverse-join (over-complete MPN) ---")
        env = call_find(server, {"query": "TLA2518IRTERQ1"})
        t = top(env)
        all_ok &= check("U8 still matches (base-match other direction)",
                        t and t["refdes"] == "U8" and "base-match" in t["match_reason"])

        print("\n--- value ('10k') ---")
        env = call_find(server, {"query": "10k"})
        all_ok &= check("returns 10k resistors",
                        env["returned"] > 0
                        and all("10k" in c["value"].lower() for c in env["candidates"]))
        all_ok &= check("sorted by confidence desc", sorted_desc(env))

        print("\n--- functional ('adc') — page-name match, IC ranks first ---")
        # U8's only "adc" text is its /ADC1/ sheet, tying it at 0.55 with every
        # decoupling cap on the ADC sheets. The pin-count tie-break lifts the
        # 16-pin ADC above the 2-pin caps, so the primary part tops the list —
        # what a generic agent asking for "adc" actually wants.
        env = call_find(server, {"query": "adc"})
        t = top(env)
        all_ok &= check("an ADC IC (U8-U11) top-ranks on 'adc', not a cap",
                        t and t["refdes"].startswith("U") and t["pin_count"] > 2)
        all_ok &= check("U8 present",
                        "U8" in [c["refdes"] for c in env["candidates"]])

        print("\n--- description word ('unpolarized') — field-weighted, above sheet-only ---")
        env = call_find(server, {"query": "unpolarized"})
        t = top(env)
        all_ok &= check("top hit scores via description, above sheet weight (0.25)",
                        t and t["confidence"] > 0.25 and "description" in t["match_reason"])

        print("\n--- every candidate carries a reason + sorted ---")
        env = call_find(server, {"query": "spi"})
        all_ok &= check("all have non-empty match_reason",
                        all(c.get("match_reason") for c in env["candidates"]))
        all_ok &= check("sorted by confidence desc", sorted_desc(env))

        print("\n--- edge: blank query ---")
        env = call_find(server, {"query": "   "})
        all_ok &= check("blank query -> returned 0, empty candidates",
                        env["returned"] == 0 and env["candidates"] == [])

        print(f"\n{'ALL PASS' if all_ok else 'SOME FAILED'}")
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

    sys.exit(0 if all_ok else 1)


if __name__ == "__main__":
    main()
