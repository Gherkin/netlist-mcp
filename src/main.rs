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
struct PinsOnNetParams {
    /// Net name, e.g. "SPI_CLK" or "/power/+3V3"
    net: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NetOfPinParams {
    /// Pin as "REFDES:PIN", e.g. "U1:5"
    pin: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Comp {
    /// Component as "REFDES", e.g. "U1"
    refdes: String,
}

#[derive(Clone)]
struct NetlistServer {
    design: std::sync::Arc<Design>,
}

#[tool_router(server_handler)]
impl NetlistServer {
    #[tool(description = "List all pins connected to a net. \
        Pins are returned as REFDES:PIN, e.g. 'U1:5'.")]
    fn get_pins(&self, Parameters(p): Parameters<PinsOnNetParams>) -> String {
        match self.design.pins_on_net(&p.net) {
            Ok(pins) => pins.join(", "),
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Get the net a pin is connected to. \
        Pin format is REFDES:PIN, e.g. 'U1:5'.")]
    fn get_net(&self, Parameters(p): Parameters<NetOfPinParams>) -> String {
        match self.design.net_of_pin(&p.pin) {
            Ok(net) => net,
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(description = "Get the properties of a component \
        format is REFDES, e.g. 'U1'.")]
    fn get_comp(&self, Parameters(p): Parameters<Comp>) -> String {
        match self.design.comp_details(&p.refdes) {
            Ok(comp) => comp,
            Err(e) => format!("error: {e:#}"),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let design = load_design()?;
dd    println!("{:?}", design.comp_details(&"U1".to_string()));
    let service = NetlistServer { design: Arc::new(design) }
        .serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}