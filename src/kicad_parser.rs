use anyhow::{Context, ensure};
use anyhow::bail;

use crate::netlist;

#[derive(Debug)]
#[derive(PartialEq)]
#[derive(Clone)]
enum Symbol {
    ParenLeft,
    ParenRight,
    Export,
    Version,
    Design,
    Source,
    Date,
    Tool,
    Sheet,
    Number,
    Name,
    Names,
    Tstamps,
    TitleBlock,
    Title,
    Company,
    Rev,
    Comment,
    Value,
    Components,
    Comp,
    Ref,
    Footprints,
    Footprint,
    Description,
    Fields,
    Field,
    Units,
    Unit,
    Pins,
    PinType,
    PinFunction,
    Pin,
    LibSource,
    Libraries,
    Library,
    Logical,
    Lib,
    Parts,
    Part,
    Property,
    Path,
    Num,
    Datasheet,
    Groups,
    Variants,
    Docs,
    Fp,
    Type,
    Uri,
    Nets,
    Net,
    Code,
    Class,
    Node,
    Function,
    Val(String)
}

#[derive(Debug)]
#[derive(PartialEq)]
enum NetListNode {
    Symbol(Symbol),
    Value(String),
    List(Vec<NetListNode>)
}

impl NetListNode {
    pub fn as_symbol(&self) -> anyhow::Result<&Symbol> {
        match self {
            NetListNode::Symbol(sym) => {
                return Ok(sym);
            }
            _ => {
                bail!("NetListNode is not a Symbol");
            }
        }
    }

    pub fn as_list(&self) -> anyhow::Result<&Vec<NetListNode>> {
        match self {
            NetListNode::List(list) => {
                return Ok(list);
            }
            _ => {
                bail!("NetListNode is not a List");
            }
        }
    }

    pub fn as_val(&self) -> anyhow::Result<&String> {
        match self {
            NetListNode::Value(val) => {
                return Ok(val);
            }
            _ => {
                bail!("NetListNode is not a Value");
            }
        }
    }

    pub fn key(&self) -> anyhow::Result<&Symbol> {
        let NetListNode::List(list) = self else {
            bail!("NetListNode is not a List");
        };

        let NetListNode::Symbol(sym) = &list[0] else {
            bail!("First element on NetListNode::List is not a Symbol")
        };

        return Ok(sym);
    }

    pub fn list(&self) -> anyhow::Result<&[NetListNode]> {
        let NetListNode::List(list) = self else {
            bail!("NetListNode is not a List");
        };

        return list.get(1..).with_context(|| format!("List was too short with length {}", list.len()));

    }
    
    pub fn get_child(&self, child: &Symbol) -> anyhow::Result<Vec<&NetListNode>> {
        match self {
            NetListNode::Symbol(sym) => {
                if sym == child {
                    return Ok(vec![self]);
                };
                bail!("NetListNode was leaf Symbol but was {:?} instead of {:?}", sym, child);
            }
            NetListNode::Value(_) => {
                bail!("Leaf NetListNode was Value");
            }
            NetListNode::List(_) => {
                let sym = self.key()?;

                if sym == child {
                    return Ok(vec![self]);
                };

                let result: Vec<&NetListNode> = self.list()?
                    .into_iter()
                    .map(|x| x.get_child(child))
                    .filter_map(Result::ok)
                    .flatten()
                    .collect();

                return Ok(result);
            }
        }

    }

    pub fn get_child_val(&self, child: &Symbol) -> anyhow::Result<Vec<&String>> {
        let child_nodes = self.get_child(child)?;
        let list: anyhow::Result<Vec<&String>> = child_nodes
            .into_iter()
            .map(|x| -> anyhow::Result<&String> {
                let items = x.list()?;
                let first = items.get(0).context("child node had no value")?;
                first.as_val()
            })
            .collect();
        
        return list;
    }

}

fn print_node(node: &NetListNode, depth: usize) {
    let indent = "  ".repeat(depth);
    match node {
        NetListNode::Symbol(a) => {
            println!("{indent}{:?}", a);
        }
        NetListNode::Value(a) => {
            println!("{indent}\"{}\"", a);
        }
        NetListNode::List(rest) => {
            println!("{indent}[");
            for e in rest {
                print_node(e, depth + 1);
            }
            println!("{indent}]");
        }
    }
}

fn parse_component(node: &NetListNode) -> anyhow::Result<netlist::Component> {
    let sym = node.key()?;
    ensure!(*sym == Symbol::Comp, "NetListNode passed was not Symbol::Comp but {sym:?}");

    let mut comp = netlist::Component::new();
    comp.refdes = node.get_child_val(&Symbol::Ref)?.get(0)
        .context("Comp Ref list was empty")?
        .to_string();
    comp.value = node.get_child_val(&Symbol::Value)?.get(0)
        .context("Comp Value list was empty")?
        .to_string();
    comp.footprint = node.get_child_val(&Symbol::Footprint)?.get(0)
        .context("Comp Footprint list was empty")?
        .to_string();

    let pins: anyhow::Result<Vec<netlist::Pin>> = node.get_child(&Symbol::Pin)?
        .into_iter()
        .map(|x| -> anyhow::Result<netlist::Pin> {
           Ok(netlist::Pin { 
            number: x.get_child_val(&Symbol::Num)?
                .get(0)
                .with_context(|| format!("Pin {x:?} had no number"))?
                .to_string(), 
            name: x.get_child_val(&Symbol::Name)?
                .get(0)
                .map(|x| x.to_string()),
            net: None
        })})
        .collect();

    comp.pins = pins?;

    return Ok(comp);
}

fn parse_net(node: &NetListNode, comps: &mut Vec::<netlist::Component>) -> anyhow::Result<netlist::Net> {
    let sym = node.key()?;
    ensure!(*sym == Symbol::Net, "NetListNode passed was not Symbol::Comp but {sym:?}");

    let code = node.get_child_val(&Symbol::Code)?
        .get(0)
        .context("Net Code list was empty")?
        .to_string()
        .parse::<usize>()?;

    let name = node.get_child_val(&Symbol::Name)?
        .get(0)
        .context("Net Name list was empty")?
        .to_string();

    let net = netlist::Net {
        code: code,
        name: name
    };


    for node in node.get_child(&Symbol::Node)? {
        let node_refdes = node.get_child_val(&Symbol::Ref)
            .with_context(|| format!("Couldnt find refdes value of node {:?} on net {}", node, net.name))?
            .get(0)
            .with_context(|| format!("Net {} had a node with empty ref", net.name))?
            .to_string();

        let node_pin = node.get_child_val(&Symbol::Pin)
            .with_context(|| format!("Couldnt find pin value of node {:?} on net {}", node, net.name))?
            .get(0)
            .with_context(|| format!("Net {} had a node with empty pin", net.name))?
            .to_string();

        let comp = comps.iter_mut()
            .find(|x| x.refdes == node_refdes)
            .with_context(|| format!("Node component {} in net {} has no corresponding component in component list", node_refdes, net.name))?;
        
        let pin = comp.pins
            .iter_mut()
            .find(|x| x.number == node_pin)
            .with_context(|| format!("Node component {} pin {} in net {} has no corresponding pin in component list", node_refdes, node_pin, net.name))?;

        pin.net = Some(net.code.clone());


    }

    return Ok(net);

}

fn scan_next(data: &mut &str) -> Option<Symbol> {
    if data.len() == 0 {
        return None;
    }
    
    // Check first char
    let c = &data[..1];
    if c == "(" {
        *data = &data[1..];
        return Some(Symbol::ParenLeft);
    } else if c == ")" {
        *data = &data[1..];
        return Some(Symbol::ParenRight);
    }

    let mut i = 1;
    loop {
        i += 1;
        let sub = &data[..i];

        let sym: Option<Symbol> = match sub {
            "export" => Some(Symbol::Export),
            "version" => Some(Symbol::Version),
            "design" => Some(Symbol::Design),
            "source" => Some(Symbol::Source),
            "date"   => Some(Symbol::Date),
            "tool"   => Some(Symbol::Tool),
            "sheet"  => Some(Symbol::Sheet),
            "number" => Some(Symbol::Number),
            sub if sub == "num" && &data[i..i + 1] != "b" => Some(Symbol::Num),
            "names"   => Some(Symbol::Names),
            sub if sub == "name" && &data[i..i + 1] != "s" => Some(Symbol::Name),
            "tstamps" => Some(Symbol::Tstamps),
            "title_block" => Some(Symbol::TitleBlock),
            "comment" => Some(Symbol::Comment),
            "value" => Some(Symbol::Value),
            "components" => Some(Symbol::Components),
            "ref" => Some(Symbol::Ref),
            "footprints" => Some(Symbol::Footprints),
            sub if sub == "footprint" && &data[i..i + 1] != "s" => Some(Symbol::Footprint),
            "description" => Some(Symbol::Description),
            "fields" => Some(Symbol::Fields),
            sub if sub == "field" && &data[i..i + 1] != "s" => Some(Symbol::Field),
            "units" => Some(Symbol::Units),
            sub if sub == "unit" && &data[i..i + 1] != "s" => Some(Symbol::Unit),
            "pins" => Some(Symbol::Pins),
            "pintype" => Some(Symbol::PinType),
            "pinfunction" => Some(Symbol::PinFunction),
            sub if sub == "pin" && &data[i..i + 1] != "s" && &data[i..i + 1] != "t" && &data[i..i + 1] != "f" => Some(Symbol::Pin),
            "parts" => Some(Symbol::Parts),
            sub if sub == "part" && &data[i..i + 1] != "s" => Some(Symbol::Part),
            "nets" => Some(Symbol::Nets),
            sub if sub == "net" && &data[i..i + 1] != "s" => Some(Symbol::Net),
            "property" => Some(Symbol::Property),
            "path" => Some(Symbol::Path),
            "datasheet" => Some(Symbol::Datasheet),
            "groups" => Some(Symbol::Groups),
            "variants" => Some(Symbol::Variants),
            "docs" => Some(Symbol::Docs),
            "fp" => Some(Symbol::Fp),
            "type" => Some(Symbol::Type),
            "logical" => Some(Symbol::Logical),
            "uri" => Some(Symbol::Uri),
            "code" => Some(Symbol::Code),
            "class" => Some(Symbol::Class),
            "libsource" => Some(Symbol::LibSource),
            "libraries" => Some(Symbol::Libraries),
            "library" => Some(Symbol::Library),
            "node" => Some(Symbol::Node),
            "function" => Some(Symbol::Function),
            sub if sub == "lib" && &data[i..i + 1] != "s" && &data[i..i + 1] != "r" => Some(Symbol::Lib),
            sub if sub == "comp" && &data[i..i + 1] != "a" && &data[i..i + 1] != "o" => Some(Symbol::Comp),
            sub if sub == "title" && &data[i..i + 1] != "_" => Some(Symbol::Title),
            "company" => Some(Symbol::Company),
            "rev" => Some(Symbol::Rev),
            sub if sub.starts_with("\"") && sub.ends_with("\"") => Some(Symbol::Val((&sub[1..sub.len() - 1]).to_string())),
            _ => None,
        };

        match sym {
            Some(symbol) => {
                *data = &data[i..];
                return Some(symbol)
            },
            None => continue,
        };
    }
}

fn structurize(syms: &mut &[Symbol]) -> NetListNode {
    let Symbol::ParenLeft = syms[0] else { 
        panic!("no left paranthesis in structurize, misaligned")
    };

    *syms = &syms[1..];

    let key;
    match syms[0].clone() {
        Symbol::ParenLeft => {
            panic!("two left paranthesis after each other in structurize!");
        }
        Symbol::ParenRight => {
            panic!("empty node in structurize!");
        }
        Symbol::Val(v) => {
            *syms = &syms[1..];
            key = NetListNode::Value(v.clone());
        }
        x => {
            *syms = &syms[1..];
            key = NetListNode::Symbol(x.clone());
        },
    };

    let mut val: Vec<NetListNode> = Vec::new();
    val.push(key);
    loop {
        let elem = match syms[0].clone() {
            Symbol::ParenLeft => structurize(syms),
            Symbol::ParenRight => {
                *syms = &syms[1..];
                break;
            }
            Symbol ::Val(val) => {
                *syms = &syms[1..];
                NetListNode::Value(val)
            }
            x => {
                *syms = &syms[1..];
                NetListNode::Symbol(x)
            }
        };
        val.push(elem)
    }

    if val.len() < 2 {
        return val.pop().unwrap();
    } else {
        return NetListNode::List(val);
    }

}

fn parse_symbol_tree(data: &String) -> Vec<Symbol> {
    let mut cursor: &str = &data;

    let mut syms: Vec<Symbol> = Vec::new();
    loop {
        let sym = scan_next(&mut cursor);
        match sym {
            Some(sym) => syms.push(sym),
            None => break,
        }
    }
    return syms;
}

pub fn parse_netlist(data: &String) -> anyhow::Result<netlist::Netlist> {
    let syms = parse_symbol_tree(data);

    let mut slice: &[Symbol] = &syms;
    let nodetree : NetListNode = structurize(&mut slice);

    let mut comps = nodetree.get_child(&Symbol::Comp)?
        .into_iter()
        .map(parse_component)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let nets = nodetree.get_child(&Symbol::Net)?
        .into_iter()
        .map(|n| parse_net(n, &mut comps)) 
        .collect::<anyhow::Result<Vec<_>>>()?;

    let netlist = netlist::Netlist {
        components: comps,
        nets: nets
    };

    return Ok(netlist);
}