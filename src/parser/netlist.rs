use core::fmt;
use std::collections::HashMap;

#[derive(Debug)]
pub struct Netlist {
    pub components: Vec<Component>,
    pub nets: Vec<Net>
}

#[derive(Debug)]
pub struct Component {
    pub refdes: String,
    pub value: String,
    pub footprint: Option<String>,
    pub description: Option<String>,
    pub sheet: Option<String>,
    pub properties: HashMap<String, Option<String>>,
    pub pins: Vec<Pin>
}

impl Component {
    pub fn new() -> Component {
        return Component {
            refdes: String::new(),
            value: String::new(),
            footprint: None,
            description: None,
            sheet: None,
            properties: HashMap::new(),
            pins: Vec::new()
        }
    }
}

#[derive(Debug)]
pub struct Pin {
    pub number: String,
    pub name: Option<String>,
    pub net: Option<usize>
}

impl Pin {
    pub fn new() -> Pin {
        return Pin {
            number: String::new(),
            name: None,
            net: None
        };
    }
}
#[derive(Debug)]
pub struct Net {
    pub code: usize,
    pub name: String
}