use core::fmt;
use std::collections::HashMap;

pub struct NetList {
    components: Vec<Component>,
    nets: Vec<Net>
}

#[derive(Debug)]
pub struct Component {
    pub refdes: String,
    pub value: String,
    pub footprint: String,
    pub properties: HashMap<String, String>,
    pub pins: Vec<Pin>
}

impl Component {
    pub fn new() -> Component {
        return Component {
            refdes: String::new(),
            value: String::new(),
            footprint: String::new(),
            properties: HashMap::new(),
            pins: Vec::new()
        }
    }
}

#[derive(Debug)]
pub struct Pin {
    pub number: String,
    pub name: Option<String>,
    pub net: Option<Box<Net>>
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
    name: String
}