#!/usr/bin/env python3
"""Generate `tests/fixture.net`, the netlist the test scripts run against.

A small fictional board, written in KiCad's `Eeschema 10` export format, whose
only purpose is to exercise the classification logic — and, deliberately, every
open issue's failure mode. Regenerate with:

    python3 tests/make_fixture.py

Notes on the format, learned from a real export and from `src/parser/`:

  * A `comp` is matched to its `libpart` by (lib, part); exactly one must match.
  * Pin *names* and *types* come from the libpart. The `pintype` on a net's
    `node` is ignored by the parser, so type coverage lives in `libparts`.
  * A `comp`'s own pins are bare `(pin (num "N"))` under `units/unit/pins`.
  * KiCad emits `(property (name "dnp"))` with the name only and **no**
    `(value ...)`, and emits it *only for the parts that carry the flag*. The
    presence of the key is the flag (issue #15). `exclude_from_bom` works the
    same way and is an independent flag — R3 below is DNP but still in the BOM,
    H2/TP* are in the BOM's exclusion list but populated.
"""

from pathlib import Path

OUT = Path(__file__).resolve().parent / "fixture.net"

# --- library parts -------------------------------------------------------
# name -> (lib, part, description, [(num, pin_name, pin_type), ...])
LIBPARTS = {
    "R":       ("Device", "R", "Resistor", [("1", "~", "passive"), ("2", "~", "passive")]),
    "C":       ("Device", "C", "Unpolarized capacitor", [("1", "~", "passive"), ("2", "~", "passive")]),
    "L":       ("Device", "L", "Inductor", [("1", "~", "passive"), ("2", "~", "passive")]),
    "FB":      ("Device", "FerriteBead", "Ferrite bead", [("1", "~", "passive"), ("2", "~", "passive")]),
    "TP":      ("Connector", "TestPoint", "Test point", [("1", "1", "passive")]),
    "MTG":     ("Mechanical", "MountingHole", "Mounting hole", [("1", "1", "passive")]),
    # Regulator: power_in / power_out, plus an open_collector PG (issue #14's
    # verified half) and an open_emitter FLAG (the half real boards rarely have).
    "LDO":     ("Regulator", "AP2112K", "300mA LDO regulator", [
                    ("1", "VIN", "power_in"), ("2", "GND", "power_in"),
                    ("3", "EN", "input"), ("4", "PG", "open_collector"),
                    ("5", "VOUT", "power_out"), ("6", "FLAG", "open_emitter")]),
    # Quad opamp: four independent channels (the multi-channel case that makes
    # naive in->out passthrough wrong — issue #12's motivation).
    "OPAMP4":  ("Amplifier", "TLV9004", "Quad op amp", [
                    ("1", "OUTA", "output"), ("2", "INA-", "input"), ("3", "INA+", "input"),
                    ("4", "V+", "power_in"), ("5", "INB+", "input"), ("6", "INB-", "input"),
                    ("7", "OUTB", "output"), ("8", "OUTC", "output"), ("9", "INC-", "input"),
                    ("10", "INC+", "input"), ("11", "V-", "power_in"), ("12", "IND+", "input"),
                    ("13", "IND-", "input"), ("14", "OUTD", "output")]),
    # PHY with dual-function pin names containing a slash (issue #16's shape).
    "PHY":     ("Interface", "LAN8720A", "Ethernet PHY", [
                    ("1", "VDDCR", "power_in"), ("2", "GND", "power_in"),
                    ("3", "LED1/nREGOFF", "bidirectional"), ("4", "RXD0/MODE0", "bidirectional"),
                    ("5", "XTAL1", "input"), ("6", "XTAL2", "output"),
                    ("7", "TXP", "bidirectional"), ("8", "TXN", "bidirectional"),
                    ("9", "RXP", "input"), ("10", "RXN", "input")]),
    # Endpoint classes the tool does not recognize today (issue #13).
    "RJ45":    ("Connector", "RJ45-Magjack", "RJ45 jack", [
                    ("1", "TX+", "unspecified"), ("2", "TX-", "unspecified"),
                    ("3", "RX+", "unspecified"), ("6", "RX-", "unspecified")]),
    "MAGS":    ("Transformer", "H1102NL", "Ethernet magnetics", [
                    ("1", "TD+", "unspecified"), ("2", "TD-", "unspecified"),
                    ("3", "RD+", "unspecified"), ("6", "RD-", "unspecified"),
                    ("9", "MX1+", "unspecified"), ("10", "MX1-", "unspecified")]),
    "XTAL":    ("Device", "Crystal_GND24", "Crystal", [
                    ("1", "1", "unspecified"), ("2", "2", "unspecified"),
                    ("3", "3", "unspecified"), ("4", "4", "unspecified")]),
    "CONN2":   ("Connector", "Conn_01x02", "2-pin header", [
                    ("1", "Pin_1", "passive"), ("2", "Pin_2", "passive")]),
    "DIODE":   ("Device", "D_Schottky", "Schottky diode", [
                    ("1", "K", "passive"), ("2", "A", "passive")]),
    "NMOS":    ("Device", "Q_NMOS_GSD", "N-channel MOSFET", [
                    ("1", "G", "input"), ("2", "S", "passive"), ("3", "D", "passive")]),
    "SW":      ("Switch", "SW_Push", "Push button", [("1", "1", "passive"), ("2", "2", "passive")]),
}

# KiCad symbols carry keywords distinct from the description, and the two are
# weighted differently by find_components — keep them different here so both
# paths are exercised.
KEYWORDS = {
    "R": "R res resistor", "C": "cap capacitor", "L": "inductor choke coil",
    "FB": "ferrite bead emi filter", "TP": "test point", "MTG": "mounting hole",
    "LDO": "linear regulator ldo", "OPAMP4": "opamp operational amplifier",
    "PHY": "ethernet phy rmii", "RJ45": "ethernet jack magjack",
    "MAGS": "ethernet magnetics transformer", "XTAL": "crystal resonator",
    "CONN2": "connector header", "DIODE": "diode schottky rectifier",
    "NMOS": "mosfet nmos transistor", "SW": "switch button",
}

# --- components ----------------------------------------------------------
# refdes -> (libpart key, value, sheet, dnp?, exclude_from_bom?)
COMPONENTS = {}
# net name -> [(refdes, pin), ...]
NETS = {}


def comp(refdes, part, value, sheet, dnp=False, efb=None):
    # Mechanical parts and test points are the usual BOM exclusions. Default
    # them that way, but keep `efb` explicit so the two flags can be set
    # independently of each other (issue #15).
    if efb is None:
        efb = part in ("MTG", "TP")
    COMPONENTS[refdes] = (part, value, sheet, dnp, efb)


def wire(net, *nodes):
    NETS.setdefault(net, []).extend(nodes)


# Root sheet "/" — real, addressable, and small (issue #17: it must be
# selectable without matching every other sheet, whose paths contain "/").
comp("H1", "MTG", "MountingHole", "/", dnp=True)          # DNP *and* excluded
comp("H2", "MTG", "MountingHole", "/")
comp("TP1", "TP", "TestPoint", "/")
comp("SW1", "SW", "SW_Push", "/")

# Power sheet.
comp("U1", "LDO", "AP2112K-3.3", "/Power/")
comp("C1", "C", "10u 25V", "/Power/")        # space before unit (issue #18)
comp("C2", "C", "100n", "/Power/")
comp("C3", "C", "33 pF", "/Power/")          # space before unit (issue #18)
comp("R1", "R", "24K", "/Power/")            # uppercase K (issue #18)
comp("R2", "R", "4k7", "/Power/")
comp("R3", "R", "100k", "/Power/", dnp=True, efb=False)   # DNP but in the BOM
comp("L1", "L", "2.2u", "/Power/")
comp("FB1", "FB", "600R", "/Power/")
comp("D1", "DIODE", "SS34", "/Power/")
comp("Q1", "NMOS", "2N7002", "/Power/")
comp("J1", "CONN2", "Barrel_Jack", "/Power/")
comp("TP2", "TP", "TestPoint", "/Power/")

wire("VIN", ("J1", "1"), ("D1", "2"), ("C1", "1"), ("FB1", "1"))
wire("VIN_F", ("FB1", "2"), ("U1", "1"), ("C2", "1"))
wire("+3V3", ("U1", "5"), ("C3", "1"), ("L1", "1"), ("TP2", "1"),
     ("U2", "1"), ("U3", "4"))
wire("PG", ("U1", "4"), ("R1", "1"))          # open_collector driver + pull-up
wire("FLAG", ("U1", "6"), ("R2", "1"))        # open_emitter driver (issue #14)
wire("EN", ("U1", "3"), ("R3", "1"), ("Q1", "3"))
wire("GATE", ("Q1", "1"), ("SW1", "1"))

# Three instances of the same sensor sheet. SENS3 is deliberately missing its
# filter cap, so its fanout differs from the other two — the asymmetry that
# issue #19 asks `audit` to surface.
comp("U3", "OPAMP4", "TLV9004", "/Sensor1/")
for i, sheet in enumerate(("/Sensor1/", "/Sensor2/", "/Sensor3/"), start=1):
    comp(f"R{10 + i}", "R", "10k", sheet)
    comp(f"R{20 + i}", "R", "10k", sheet)
    comp(f"TP{10 + i}", "TP", "TestPoint", sheet)
    if i != 3:
        comp(f"C{10 + i}", "C", "1n", sheet)
    wire(f"/SENSIN{i}", ("J2", str(i)), (f"R{10 + i}", "1"))
    divider = [(f"R{10 + i}", "2"), (f"R{20 + i}", "1"), (f"TP{10 + i}", "1")]
    if i != 3:
        divider.append((f"C{10 + i}", "1"))
    wire(f"/SENSMID{i}", *divider)
    wire("+VSENS", (f"R{20 + i}", "2"))

comp("J2", "CONN2", "Sensor_Header", "/Sensor1/")
comp("C20", "C", "1u", "/Sensor1/")
comp("R30", "R", "1k", "/Sensor1/")
# A rail whose pins are all passive and whose fanout is modest: it scores under
# the rail threshold and under walk's fanout cap (issue #3).
wire("+VSENS", ("C20", "1"), ("R30", "1"), ("U3", "12"))
wire("/SENSOUT", ("U3", "14"), ("R30", "2"))
# An input with no driver anywhere on the net (an audit `undriven_input`).
wire("/SENSREF", ("U3", "13"), ("C20", "2"))

# Ethernet sheet: RJ45 jack, magnetics and crystal — the endpoint classes the
# tool does not recognize (issue #13) — plus a net label containing a slash
# (issue #16) and a genuine stub.
comp("U2", "PHY", "LAN8720A", "/Ethernet/")
comp("RJ1", "RJ45", "RJHSE-5380", "/Ethernet/")
comp("T1", "MAGS", "H1102NL", "/Ethernet/")
comp("X1", "XTAL", "25MHz", "/Ethernet/")
comp("C30", "C", "18p", "/Ethernet/")
comp("C31", "C", "18p", "/Ethernet/")
comp("R40", "R", "1k", "/Ethernet/")
comp("R41", "R", "49R9", "/Ethernet/", dnp=True, efb=True)
comp("TP20", "TP", "TestPoint", "/Ethernet/")

wire("/Ethernet/LED1/REGOFF", ("U2", "3"), ("R40", "1"))   # slash in the label
wire("/Ethernet/MX1+", ("RJ1", "1"), ("T1", "9"), ("TP20", "1"))  # RJ + T only
wire("/Ethernet/MX1-", ("RJ1", "2"), ("T1", "10"))
wire("/Ethernet/TXP", ("U2", "7"), ("T1", "1"))
wire("/Ethernet/TXN", ("U2", "8"), ("T1", "2"))
wire("/Ethernet/RXP", ("U2", "9"), ("T1", "3"))
wire("/Ethernet/RXN", ("U2", "10"), ("T1", "6"))
wire("/Ethernet/XTAL1", ("U2", "5"), ("X1", "1"), ("C30", "1"))
wire("/Ethernet/XTAL2", ("U2", "6"), ("X1", "3"), ("C31", "1"))
wire("/Ethernet/MODE0", ("U2", "4"), ("R41", "1"))
wire("Net-(R41-Pad2)", ("R41", "2"))                        # single-pin net

# GND touches every sheet, so a subsystem filter matches it everywhere
# (issue #4).
wire("GND", ("H2", "1"), ("TP1", "1"), ("SW1", "2"),
     ("C1", "2"), ("C2", "2"), ("C3", "2"), ("U1", "2"), ("D1", "1"),
     ("Q1", "2"), ("J1", "2"), ("L1", "2"), ("R1", "2"), ("R2", "2"),
     ("R3", "2"), ("C11", "2"), ("C12", "2"), ("C20", "2"),
     ("R21", "2"), ("R22", "2"), ("R23", "2"),
     ("U2", "2"), ("U3", "11"), ("X1", "2"), ("X1", "4"),
     ("C30", "2"), ("C31", "2"), ("R40", "2"), ("TP2", "1"))

# --- emit ----------------------------------------------------------------
IND = "\t"


def esc(text):
    return text.replace("\\", "\\\\").replace('"', '\\"')


def emit_libparts():
    out = [f"{IND}(libparts"]
    for key, (lib, part, desc, pins) in LIBPARTS.items():
        out += [
            f"{IND * 2}(libpart",
            f'{IND * 3}(lib "{esc(lib)}")',
            f'{IND * 3}(part "{esc(part)}")',
            f'{IND * 3}(description "{esc(desc)}")',
            f"{IND * 3}(pins",
        ]
        for num, name, ptype in pins:
            out += [
                f"{IND * 4}(pin",
                f'{IND * 5}(num "{num}")',
                f'{IND * 5}(name "{esc(name)}")',
                f'{IND * 5}(type "{ptype}")',
                f"{IND * 4})",
            ]
        out += [f"{IND * 3})", f"{IND * 2})"]
    out.append(f"{IND})")
    return out


def emit_components():
    out = [f"{IND}(components"]
    for refdes, (part_key, value, sheet, dnp, efb) in COMPONENTS.items():
        lib, part, desc, pins = LIBPARTS[part_key]
        used = sorted({pin for (ref, pin) in
                       ((r, p) for nodes in NETS.values() for (r, p) in nodes)
                       if ref == refdes},
                      key=lambda p: (len(p), p))
        out += [
            f"{IND * 2}(comp",
            f'{IND * 3}(ref "{esc(refdes)}")',
            f'{IND * 3}(value "{esc(value)}")',
            f'{IND * 3}(footprint "Fixture:{esc(part_key)}")',
            f'{IND * 3}(description "{esc(desc)}")',
            f"{IND * 3}(libsource",
            f'{IND * 4}(lib "{esc(lib)}")',
            f'{IND * 4}(part "{esc(part)}")',
            f'{IND * 4}(description "{esc(desc)}")',
            f"{IND * 3})",
            # Presence-only flags: the name is emitted with no value, and only
            # for the parts that actually carry the flag (issue #15).
        ]
        if efb:
            out += [
                f"{IND * 3}(property",
                f'{IND * 4}(name "exclude_from_bom")',
                f"{IND * 3})",
            ]
        if dnp:
            out += [
                f"{IND * 3}(property",
                f'{IND * 4}(name "dnp")',
                f"{IND * 3})",
            ]
        out += [
            f"{IND * 3}(property",
            f'{IND * 4}(name "ki_keywords")',
            f'{IND * 4}(value "{esc(KEYWORDS[part_key])}")',
            f"{IND * 3})",
            f"{IND * 3}(sheetpath",
            f'{IND * 4}(names "{esc(sheet)}")',
            f'{IND * 4}(tstamps "{esc(sheet)}")',
            f"{IND * 3})",
            f"{IND * 3}(units",
            f"{IND * 4}(unit",
            f'{IND * 5}(name "A")',
            f"{IND * 5}(pins",
        ]
        for num in used:
            out += [
                f"{IND * 6}(pin",
                f'{IND * 7}(num "{num}")',
                f"{IND * 6})",
            ]
        out += [f"{IND * 5})", f"{IND * 4})", f"{IND * 3})", f"{IND * 2})"]
    out.append(f"{IND})")
    return out


def emit_nets():
    out = [f"{IND}(nets"]
    pin_types = {}
    for key, (_lib, _part, _desc, pins) in LIBPARTS.items():
        pin_types[key] = {num: ptype for num, _name, ptype in pins}
    for code, (name, nodes) in enumerate(sorted(NETS.items()), start=1):
        out += [
            f"{IND * 2}(net",
            f'{IND * 3}(code "{code}")',
            f'{IND * 3}(name "{esc(name)}")',
            f'{IND * 3}(class "Default")',
        ]
        for refdes, pin in nodes:
            part_key = COMPONENTS[refdes][0]
            out += [
                f"{IND * 3}(node",
                f'{IND * 4}(ref "{esc(refdes)}")',
                f'{IND * 4}(pin "{pin}")',
                f'{IND * 4}(pintype "{pin_types[part_key].get(pin, "passive")}")',
                f"{IND * 3})",
            ]
        out.append(f"{IND * 2})")
    out.append(f"{IND})")
    return out


def main():
    lines = [
        "(export",
        f'{IND}(version "E")',
        f"{IND}(design",
        f'{IND * 2}(source "fixture.kicad_sch")',
        f'{IND * 2}(date "2026-09-06T00:00:00")',
        f'{IND * 2}(tool "generated by tests/make_fixture.py")',
        f"{IND})",
    ]
    lines += emit_components()
    lines += emit_libparts()
    lines += emit_nets()
    lines.append(")")
    OUT.write_text("\n".join(lines) + "\n")
    print(f"wrote {OUT} — {len(COMPONENTS)} components, {len(NETS)} nets")


if __name__ == "__main__":
    main()
