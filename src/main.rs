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

use rmcp::{tool, tool_router, ServiceExt,
           handler::server::wrapper::Parameters, transport::stdio};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetNetParams {
    /// Net name (e.g. "SPI_CLK" or "/power/+3V3") or net code as a string.
    net: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetComponentParams {
    /// Component reference designator, e.g. "U1".
    refdes: String,
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

#[derive(Clone)]
struct NetlistServer {
    design: std::sync::Arc<Design>,
}

#[tool_router(server_handler)]
impl NetlistServer {
    #[tool(description = "Detail on one net: its member pins (REFDES:PIN). \
        Accepts a net name or a net code. (TODO: fanout, subsystem, rail score, \
        owning-component value inline, pagination.)")]
    fn get_net(&self, Parameters(p): Parameters<GetNetParams>) -> String {
        match self.design.pins_on_net(&p.net) {
            Ok(pins) => pins.join(", "),
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Detail on one pin (REFDES:PIN): the net it is on. \
        (TODO: pin name/function, electrical type, owning component.)")]
    fn get_pin(&self, Parameters(p): Parameters<GetPinParams>) -> String {
        match self.design.net_of_pin(&p.pin) {
            Ok(net) => net,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Full detail on one component: its properties. \
        (TODO: identity bundle, footprint, subsystem, pins with type, pagination.)")]
    fn get_component(&self, Parameters(p): Parameters<GetComponentParams>) -> String {
        match self.design.comp_details(&p.refdes) {
            Ok(comp) => comp,
            Err(e) => format!("error: {e:#}"),
        }
    }

    // -- Placeholders: rest of the target surface (see tools_todo.md) --------

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
    fn walk(&self, Parameters(_p): Parameters<WalkParams>) -> String {
        "not implemented".to_string()
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
    fn path_between(&self, Parameters(_p): Parameters<PathBetweenParams>) -> String {
        "not implemented".to_string()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let design = load_design()?;
    design.comp_details(&"U1".to_string());
    let service = NetlistServer { design: Arc::new(design) }
        .serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}