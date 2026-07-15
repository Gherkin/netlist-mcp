use std::error::Error;
use std::env;
use std::path::Path;
use std::fs;

mod kicad_parser;
mod netlist;

fn load_file<P: AsRef<Path>>(path: P) -> String {
    let mut data = fs::read_to_string(path).expect("this file should exist");
    data.retain(|c| !c.is_whitespace());
    return data
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Need a file!");
        return Ok(())
    }
    let data = load_file(&args[1]);
    let netlist = kicad_parser::parse_netlist(&data);
    println!("{:?}", netlist);
    return Ok(());
}