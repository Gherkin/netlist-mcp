use std::env;
use std::path::Path;
use std::fs;
use std::sync::Arc;

use anyhow::bail;

use design::Design;

mod parser;
mod design;

fn load_file<P: AsRef<Path>>(path: P) -> String {
    let data = fs::read_to_string(path).expect("this file should exist");
    return data
}

fn load_design() -> anyhow::Result<Design> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        bail!("no file argument!")
    }
    let data = load_file(&args[1]);
    let netlist = parser::kicad_parser::parse_netlist(&data)?;
    let design = design::Design::from_netlist(netlist)?;
    return Ok(design);
}

use rmcp::{tool, tool_router, tool_handler, ServerHandler, ServiceExt,
           handler::server::wrapper::Parameters, transport::stdio,
           model::{ServerInfo, ServerCapabilities}};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetNetParams {
    /// Net name (e.g. "SPI_CLK" or "/power/+3V3") or net code as a string.
    net: String,
    /// Max member pins to return (default 200).
    #[serde(default)]
    limit: Option<u32>,
    /// Member offset for pagination (default 0).
    #[serde(default)]
    offset: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetComponentParams {
    /// Component reference designator, e.g. "U1".
    refdes: String,
    /// Max pins to return (default 200).
    #[serde(default)]
    limit: Option<u32>,
    /// Pin offset for pagination (default 0).
    #[serde(default)]
    offset: Option<u32>,
}

// ---------------------------------------------------------------------------
// Placeholder tool parameters for the rest of the target surface (see
// tools_todo.md). Doc comments become the JSON-schema field descriptions the
// agent sees. Handlers below return "not implemented" until wired up.
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FilterComponentsParams {
    /// Substring/all-terms match over refdes, value, description, keywords and footprint.
    #[serde(default)]
    query: Option<String>,
    /// Restrict to a refdes class, e.g. "U", "R", "C", "J".
    #[serde(default)]
    refdes_class: Option<String>,
    /// Restrict to a subsystem / sheet, e.g. "power".
    #[serde(default)]
    subsystem: Option<String>,
    /// Max rows to return (default 50).
    #[serde(default)]
    limit: Option<u32>,
    /// Row offset for pagination (default 0).
    #[serde(default)]
    offset: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FilterNetsParams {
    /// Substring / pattern to match against net names.
    #[serde(default)]
    name: Option<String>,
    /// Restrict to a subsystem / sheet.
    #[serde(default)]
    subsystem: Option<String>,
    /// Sort by descending pin fanout (default true).
    #[serde(default)]
    sort_by_fanout: Option<bool>,
    /// Max rows to return (default 50).
    #[serde(default)]
    limit: Option<u32>,
    /// Row offset for pagination (default 0).
    #[serde(default)]
    offset: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FindComponentsParams {
    /// What to look for — a part identity or a rough description, e.g. an MPN
    /// from the datasheet store ("ADS1115IDGSR"), a value ("10k"), or a loose
    /// idea ("the MCU"). Matching is fuzzy; returns ranked candidates with a
    /// confidence, best first.
    query: String,
    /// Max candidates to return (default 10).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetPinParams {
    /// Pin as "REFDES:PIN", e.g. "U7:22".
    pin: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WalkParams {
    /// Start point: a pin "REFDES:PIN" or a net name.
    start: String,
    /// Max passthrough hops before a branch is cut (default 4).
    #[serde(default)]
    max_depth: Option<u32>,
    /// Max endpoints to return (default 50).
    #[serde(default)]
    max_endpoints: Option<u32>,
    /// Stop at power/ground rails instead of enumerating them (default true).
    #[serde(default)]
    stop_at_power: Option<bool>,
    /// Return full branch topology rather than just endpoint paths (default false).
    #[serde(default)]
    include_topology: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NeighborsParams {
    /// Component reference designator, e.g. "U7".
    refdes: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PathBetweenParams {
    /// Source pin as "REFDES:PIN".
    from: String,
    /// Destination pin as "REFDES:PIN".
    to: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AuditParams {
    /// Max nets to return per category (default 100). Each category also
    /// reports its true `count` across the whole design.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Clone)]
struct NetlistServer {
    design: std::sync::Arc<Design>,
}

#[tool_router]
impl NetlistServer {
    #[tool(description = "Full detail on one net: identity, fanout, rail-score \
        with evidence, pin-type histogram, connected subsystems, and paginated \
        member pins (each with owning component, value, pin name/function, and \
        type). Accepts a net name or a net code.")]
    fn get_net(&self, Parameters(p): Parameters<GetNetParams>) -> String {
        let limit = p.limit.unwrap_or(200);
        let offset = p.offset.unwrap_or(0);
        match self.design.net_details(&p.net, limit, offset) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Full detail on one pin (REFDES:PIN): its name/function, \
        electrical type, owning component, and the net it sits on (name, code, \
        fanout, rail score) — or a null net if the pin is unconnected.")]
    fn get_pin(&self, Parameters(p): Parameters<GetPinParams>) -> String {
        match self.design.pin_details(&p.pin) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Full detail on one component: identity, keywords, \
        footprint, subsystem, the full property map, and paginated pins (each \
        with name, electrical type, and net).")]
    fn get_component(&self, Parameters(p): Parameters<GetComponentParams>) -> String {
        let limit = p.limit.unwrap_or(200);
        let offset = p.offset.unwrap_or(0);
        match self.design.comp_details(&p.refdes, limit, offset) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Summarize the whole design: component counts, a \
        refdes-class histogram, detected power rails (with confidence), \
        connectors, subsystems, and the highest-fanout nets. The zero-knowledge \
        first call for orienting in an unfamiliar design.")]
    fn design_overview(&self) -> String {
        match self.design.design_overview() {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "List the schematic's sheet hierarchy (subsystems) as a \
        tree with per-subsystem part counts. Subsystems can then filter \
        filter_components / filter_nets.")]
    fn list_subsystems(&self) -> String {
        match self.design.list_subsystems() {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Deterministic filter over components by refdes class, \
        subsystem, and substring/all-terms query across refdes/value/description/\
        keywords/footprint. Returns a flat, paginated list — every match, no \
        ranking. Use for enumeration/counting ('how many 0402 caps in the power \
        sheet') or as a fallback when find_components returns noise. For turning \
        a rough idea into a handle, prefer find_components.")]
    fn filter_components(&self, Parameters(p): Parameters<FilterComponentsParams>) -> String {
        let limit = p.limit.unwrap_or(50);
        let offset = p.offset.unwrap_or(0);
        match self.design.filter_components(
            p.query.as_deref(),
            p.refdes_class.as_deref(),
            p.subsystem.as_deref(),
            limit,
            offset,
        ) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Deterministic filter over nets by name substring and/or \
        subsystem, sorted by fanout (default) or name. Returns compact rows (net \
        name, code, fanout, pin-type distribution). This is where connectivity \
        words resolve — e.g. 'spi' finds the SPI nets, which find_components \
        cannot (SPI lives in net names, not component fields).")]
    fn filter_nets(&self, Parameters(p): Parameters<FilterNetsParams>) -> String {
        let sort_by_fanout = p.sort_by_fanout.unwrap_or(true);
        let limit = p.limit.unwrap_or(50);
        let offset = p.offset.unwrap_or(0);
        match self.design.filter_nets(
            p.name.as_deref(),
            p.subsystem.as_deref(),
            sort_by_fanout,
            limit,
            offset,
        ) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "The main way to locate components: give a part \
        identity or a rough description (an MPN from the datasheet store, a \
        value, or 'the MCU') and get ranked candidates with a confidence and the \
        reason each matched. Fuzzy — tolerant of partial/over-complete part \
        numbers, so it also serves as the reverse leg of the datasheet-RAG \
        handoff. Reach for this first; drop to filter_components when you need an \
        exhaustive count or the ranking is noisy.")]
    fn find_components(&self, Parameters(p): Parameters<FindComponentsParams>) -> String {
        let limit = p.limit.unwrap_or(10);
        match self.design.find_components(&p.query, limit) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Walk the connectivity from a pin or net through 2-pin \
        passthrough parts (series resistors, inductors, ferrites, caps) to the \
        real opaque endpoints (ICs, connectors). Power/ground rails terminate \
        the walk and are reported, never enumerated. Returns endpoints with the \
        parts traversed to reach each. Topological, not electrical. The primary \
        'what is actually connected to this?' tool.")]
    fn walk(&self, Parameters(p): Parameters<WalkParams>) -> String {
        let max_depth = p.max_depth.unwrap_or(4);
        let max_endpoints = p.max_endpoints.unwrap_or(50);
        let stop_at_power = p.stop_at_power.unwrap_or(true);
        let include_topology = p.include_topology.unwrap_or(false);
        match self.design.walk(
            &p.start,
            max_depth,
            max_endpoints,
            stop_at_power,
            include_topology,
        ) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "List the components one hop away from a component — \
        those sharing any net with it. Cheap orientation before a full walk.")]
    fn neighbors(&self, Parameters(p): Parameters<NeighborsParams>) -> String {
        match self.design.neighbors(&p.refdes) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Report whether two pins are connected and, if so, the \
        parts on the path between them. Expensive; prefer walk for open-ended \
        tracing.")]
    fn path_between(&self, Parameters(p): Parameters<PathBetweenParams>) -> String {
        match self.design.path_between(&p.from, &p.to) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Scan the whole net graph and report FACTUAL, \
        non-exclusive connectivity patterns worth a human's review — this is \
        an observation tool, not a defect detector: it never asserts a bug or \
        infers intent, only states what the graph looks like. Categories: \
        unpowered_power_in (has a power_in pin, no power_out source anywhere \
        on the net), undriven_input (has an input pin, no driver-typed pin on \
        the net), single_ic_pin (touches exactly one IC pin and only passives \
        otherwise), and stub (a single-pin net, or a multi-pin net with no IC \
        and no connector pin). A net can appear in several categories. Each \
        category reports its true count plus up to `limit` nets (default \
        100), so hundreds of stub/TP nets don't drown the response.")]
    fn audit(&self, Parameters(p): Parameters<AuditParams>) -> String {
        let limit = p.limit.unwrap_or(100);
        match self.design.audit(limit) {
            Ok(out) => out,
            Err(e) => format!("error: {e:#}"),
        }
    }
}

const SERVER_INSTRUCTIONS: &str = "\
Tools for exploring a KiCad electrical netlist: discover parts, search, and \
trace connectivity. Work in a funnel — orient, then locate, then inspect, then \
trace.

Handles used throughout: a component is a REFDES (e.g. \"U8\"); a pin is \
REFDES:PIN (e.g. \"U8:5\"); a net is its name (e.g. \"/ADC1/CS\", \"GND\") or its \
integer code.

ORIENT (start here when you don't know the design):
- design_overview: counts, a refdes-class histogram, detected power rails (with \
  confidence + evidence), connectors, subsystems, and the busiest nets. Your \
  first call in an unfamiliar design.
- list_subsystems: the schematic's sheets (subsystems) with part counts.

LOCATE (turn a rough idea into concrete handles):
- find_components: THE front door. Give a part identity or rough description (an \
  MPN, a value like \"10k\", or \"the MCU\") and get ranked candidates with a \
  confidence. Fuzzy and tolerant of partial/over-complete part numbers. Reach \
  for this first.
- filter_components: deterministic filter (refdes class, subsystem, substring \
  query). Use for exhaustive/counting questions (\"how many 0402 caps in the \
  power sheet\") or when find_components is noisy.
- filter_nets: find nets by name substring / subsystem. This is where \
  connectivity words resolve — \"spi\" finds SPI nets, which find_components \
  cannot (SPI lives in net names, not component fields).

INSPECT (full detail on a known handle):
- get_component (by REFDES), get_net (by name or code), get_pin (by REFDES:PIN). \
  These return structured detail: pins with electrical type, net fanout, rail \
  score, connected subsystems, etc.

TRACE (connectivity — the point of this server):
- walk: THE tool for \"what is actually connected to this pin?\". From a pin or \
  net it passes THROUGH series parts (resistors, inductors, ferrites, caps) to \
  the real endpoints (ICs, connectors), and stops at power/ground rails instead \
  of enumerating them. Topological, not electrical.
- neighbors: the parts one hop away, grouped by shared net (rails appear as one \
  capped group). Cheap orientation before a walk.
- path_between: whether two pins are connected through series parts (not across \
  rails), and the parts on the path.

REVIEW (factual patterns, not verdicts):
- audit: scans every net for FACTUAL graph patterns worth a look — unpowered \
  power_in nets, undriven input nets, single-IC-pin nets, and stub nets. It \
  reports observed states, never asserts a bug; you judge relevance. get_net's \
  `role` field carries the same classification for a single net.

Notes: a component's `value` is usually its manufacturer part number for ICs (a \
spec like \"10k\" for passives); `keywords` mirrors it. To find a part by \
function you can't match locally, look its MPN up in a datasheet source, then \
find_components that MPN here. Rail detection and fuzzy matching are heuristic — \
scores and confidences are hints, not guarantees.";

#[tool_handler]
impl ServerHandler for NetlistServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(SERVER_INSTRUCTIONS)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let design = load_design()?;
    let service = NetlistServer { design: Arc::new(design) }
        .serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}