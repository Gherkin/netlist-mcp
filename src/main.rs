use std::collections::HashMap;
use std::env;
use std::fs;
use std::error::Error;
use std::path::Path;

mod netlist;
use netlist::Component;

use crate::NetListElem::NodeElem;
use crate::Symbol::Unit;

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
    SingleNode(NetListElem),
    DoubleNode(NetListElem, NetListElem),
    ListNode(NetListElem, Vec<NetListElem>)
}

#[derive(Debug)]
#[derive(PartialEq)]
enum NetListElem {
    SymbolElem(Symbol),
    NodeElem(Box<NetListNode>),
    None
} 


fn print_node(node: &NetListNode, depth: usize) {
    let indent = "  ".repeat(depth);
    match node {
        NetListNode::SingleNode(a) => {
            println!("{indent}Single:");
            print_elem(a, depth + 1);
        }
        NetListNode::DoubleNode(a, b) => {
            println!("{indent}Double:");
            print_elem(a, depth + 1);
            print_elem(b, depth + 1);
        }
        NetListNode::ListNode(a, rest) => {
            println!("{indent}List:");
            print_elem(a, depth + 1);
            for e in rest {
                print_elem(e, depth + 1);
            }
        }
    }
}

fn parse_component(node: &NetListNode) -> netlist::Component {
    let mut comp = netlist::Component::new();

    print_node(node, 0);
    let NetListNode::ListNode(key, list) = node else {
        panic!("component wasnt list node!")
    };
    if *key != NetListElem::SymbolElem(Symbol::Comp) {
        panic!("component node wasnt component!")
    }

    for elem in list {
        let NetListElem::NodeElem(inner_node) = elem else { continue; };
        match inner_node.as_ref() {
            NetListNode::DoubleNode(key, val_elem) => {
                let NetListElem::SymbolElem(sym) = key else { continue; };

                match val_elem {
                    NetListElem::SymbolElem(val_sym) => {
                        let NetListElem::SymbolElem(val_sym) = val_elem else { continue };
                        let Symbol::Val(val) = val_sym else { continue };
                        match sym {
                            Symbol::Ref => {
                                comp.refdes = val.clone();
                            }
                            Symbol::Value => {
                                comp.value = val.clone();
                            }
                            Symbol::Footprint => {
                                comp.footprint = val.clone();
                            }
                            other => ()
                        }
                    }
                    NetListElem::NodeElem(val_node) => {
                        let NetListNode::ListNode(_, unit_list) = val_node.as_ref() else { continue };
                        let Symbol::Units = sym else { continue; }; 
                        let mut pins: Vec<netlist::Pin> = Vec::new();
                        for unit in unit_list {
                            let NetListElem::NodeElem(unit_node) = unit else { continue; };
                            let NetListNode::ListNode(_, pin_list) = unit_node.as_ref() else { continue; };
                            for pin in pin_list {
                                let NetListElem::NodeElem(pin_node) = pin else { continue; };
                                let NetListNode::DoubleNode(pin_key, pin_val) = pin_node.as_ref() else { continue; };
                                let NetListElem::SymbolElem(pin_sym) = pin_key else { continue; };
                                let Symbol::Pin = pin_sym else { continue; };

                                let NetListElem::NodeElem(pin_val_node) = pin_val else { continue; };
                                let NetListNode::DoubleNode(_, pin_val_val_elem) = pin_val_node.as_ref() else { continue; };
                                let NetListElem::SymbolElem(pin_val_val_sym) = pin_val_val_elem else { continue; };
                                let Symbol::Val(actual_val) = pin_val_val_sym else { continue; };
                                let mut pin = netlist::Pin::new();
                                pin.name = actual_val.clone();
                                pins.push(pin);


                            }
                        }
                        comp.pins = pins;

                    }
                    other => ()
                }
            }
            NetListNode::ListNode(key, list) => {
                // TODO handle props
                let NetListElem::SymbolElem(sym) = key else { continue; };

            }
            NetListNode::SingleNode(_) => { continue; }
        }
    }
    println!("{:?}", comp);
    return comp;
}

fn print_elem(elem: &NetListElem, depth: usize) {
    let indent = "  ".repeat(depth);
    match elem {
        NetListElem::SymbolElem(sym) => println!("{indent}{:?}", sym),
        NetListElem::NodeElem(boxed_node) => print_node(boxed_node, depth),
        NetListElem::None => println!("{indent}None"),
    }
}

fn load_file<P: AsRef<Path>>(path: P) -> String {
    let mut data = fs::read_to_string(path).expect("this file should exist");
    data.retain(|c| !c.is_whitespace());
    return data
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
            sub if sub == "pin" && &data[i..i + 1] != "s" => Some(Symbol::Pin),
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
    if syms[0] != Symbol::ParenLeft {
        println!("no left paren!");
    }
    *syms = &syms[1..];

    let key = match syms[0].clone() {
        Symbol::ParenLeft => NetListElem::NodeElem(Box::new(structurize(syms))),
        x => {
            *syms = &syms[1..];
            NetListElem::SymbolElem(x)
        },
    };

    let mut val: Vec<NetListElem>= Vec::new();
    loop {
        let elem = match syms[0].clone() {
            Symbol::ParenLeft => NetListElem::NodeElem(Box::new(structurize(syms))),
            Symbol::ParenRight => {
                *syms = &syms[1..];
                NetListElem::None
            },
            x => {
                *syms = &syms[1..];
                NetListElem::SymbolElem(x)
            },
        };
        if elem == NetListElem::None {
            break;
        }
        val.push(elem)
    }

    if val.len() < 1 {
        return NetListNode::SingleNode(key);
    } else if val.len() == 1 {
        return NetListNode::DoubleNode(key, val.remove(0));
    } else {
        return NetListNode::ListNode(key, val);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Need a file!");
        return Ok(())
    }
    let data = load_file(&args[1]);
    let mut cursor: &str = &data;

    let mut syms: Vec<Symbol> = Vec::new();
    loop {
        let sym = scan_next(&mut cursor);
        match sym {
            Some(sym) => syms.push(sym),
            None => break,
        }
    }
    println!("symbols: {}", syms.len());
    let mut slice: &[Symbol] = &syms;
    let nodetree : NetListNode = structurize(&mut slice);
    //print_node(&nodetree, 0);

    let NetListNode::ListNode(_, list) = nodetree else {
            print_node(&nodetree, 0);
            panic!("Base node of kicad netlist wasnt list!");
    };

    for e in list {
        let NetListElem::NodeElem(node) = e else {
            continue;
        };

        let NetListNode::ListNode(key, comp_list) = *node else {
            continue;
        };

        if key != NetListElem::SymbolElem(Symbol::Components) {
            continue;
        }

        for comp_elem in comp_list {
            let NetListElem::NodeElem(comp_node) = comp_elem else { break; };
            parse_component(&comp_node);
            break;
        }

    }

    

    return Ok(());
}
