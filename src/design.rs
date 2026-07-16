use std::collections::HashMap;
use std::fmt;
use std::fmt::format;
use std::fmt::Display;
use anyhow::Context;

use crate::parser::netlist;

#[derive(Debug)]
pub struct Design {
    components: Vec<Component>,
    pins: Vec<Pin>,
    nets: Vec<Net>,
    // Map of Refdes -> CompId
    component_map: HashMap<String, CompId>,
    // Map of Refdes:PinNo -> PinId
    pin_map: HashMap<String, PinId>,
    // Map of NetName -> NetId
    net_map: HashMap<String, NetId>
}

impl Design {
    pub fn pin(&self, pin_id: &PinId) -> &Pin {
        &self.pins[pin_id.0 as usize]
    }

    pub fn pin_name(&self, pin_id: &PinId) -> String {
        let pin = self.pin(pin_id);
        let comp = self.component(&pin.comp);

        return format!("{}:{}", comp.refdes, pin.number);
    }

    fn pin_from_name(&self, pin_name: &String) -> anyhow::Result<&Pin> {
        let pin_id = self.pin_map.get(pin_name).with_context(|| format!("no pin named '{}'", pin_name))?;

        return Ok(self.pin(pin_id));
    }

    pub fn net(&self, net_id: &NetId) -> &Net {
        &self.nets[net_id.0 as usize]
    }

    fn net_from_name(&self, net_name: &String) -> anyhow::Result<&Net> {
        let net_id = self.net_map.get(net_name).with_context(|| format!("no net called '{}'", net_name))?;

        return Ok(self.net(net_id))
    }
    
    pub fn component(&self, comp_id: &CompId) -> &Component {
        &self.components[comp_id.0 as usize]
    }

    pub fn pins_on_net(&self, net_name: &String) -> anyhow::Result<Vec<String>> {
        let net = self.net_from_name(net_name)?;

        let pin_names = net.pins
            .iter()
            .map(|x| self.pin_name(x))
            .collect();

        return Ok(pin_names);
    }

    pub fn net_of_pin(&self, pin_name: &String) -> anyhow::Result<String> {
        let pin = self.pin_from_name(pin_name)?;
        
        // Fix if None should mean NC
        let net_id = pin.net.as_ref().with_context(|| format!("no net for pin {}", pin_name))?;

        return Ok(self.net(net_id).name.clone());
    }

    pub fn from_netlist(netlist: netlist::Netlist) -> anyhow::Result<Design> {
        let mut nets: Vec<Net> = Vec::new();
        let mut net_map: HashMap<String, NetId> = HashMap::new();
        for (i, netlist_net) in netlist.nets.into_iter().enumerate() {
            let net = Net {
                id: NetId(i),
                code: netlist_net.code,
                name: netlist_net.name,
                pins: Vec::new()
            };
            net_map.insert(net.name.clone(), NetId(i));
            nets.push(net);
        }

        let mut comps: Vec<Component> = Vec::new();
        let mut comp_map: HashMap<String, CompId> = HashMap::new();
        let mut pins: Vec<Pin> = Vec::new();
        let mut pin_map: HashMap<String, PinId> = HashMap::new();
        let mut j: usize = 0;
        for (i, netlist_comp) in netlist.components.into_iter().enumerate() {
            let mut comp = Component {
                id: CompId(i),
                refdes: netlist_comp.refdes,
                value: netlist_comp.value,
                footprint: netlist_comp.footprint,
                properties: HashMap::new(),
                pins: Vec::new()
            };
            comp_map.insert(comp.refdes.clone(), CompId(i));

            for netlist_pin in netlist_comp.pins {
                let net_no = netlist_pin.net.context("no net code for pin!")?;
                let net_id = nets
                    .iter()
                    .position(|y: &Net| net_no == y.code)
                    .with_context(|| format!("couldnt find net {} for pin {:?} on component {:?}", net_no, netlist_pin, comp))?;

                let pin = Pin {
                    id: PinId(j),
                    comp: CompId(i),
                    number: netlist_pin.number,
                    name: netlist_pin.name,
                    net: Some(NetId(net_id))
                };
                pin_map.insert(format!("{}:{}", comp.refdes.clone(), pin.number.clone()), PinId(j));
                pins.push(pin);


                comp.pins.push(PinId(j));
                j += 1;
            }
            comps.push(comp);

        }

        for i in 0..pins.len() {
            let pin = &pins[i];

            let Some(net_no) = &pin.net else {
                continue;
            };

            nets[net_no.0 as usize].pins.push(PinId(i));

        }

        return Ok(Design {
            components: comps,
            component_map: comp_map,
            pins: pins,
            pin_map: pin_map,
            nets: nets,
            net_map: net_map
        });
    }
}

#[derive(Debug)]
pub struct CompId(usize);

#[derive(Debug)]
pub struct Component {
    id: CompId,
    refdes: String,
    value: String,
    footprint: Option<String>,
    properties: HashMap<String, String>,
    pins: Vec<PinId>
}

#[derive(Debug)]
pub struct PinId(usize);

#[derive(Debug)]
pub struct Pin {
    pub id: PinId,
    pub comp: CompId,
    pub number: String,
    pub name: Option<String>,
    pub net: Option<NetId>
}

#[derive(Debug)]
pub struct NetId(usize);

#[derive(Debug)]
pub struct Net {
    pub id: NetId,
    pub code: usize,
    pub name: String,
    pub pins: Vec<PinId>
}