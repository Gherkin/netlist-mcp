use std::collections::HashMap;

use anyhow::{Context, ensure};
use anyhow::bail;

use crate::parser::netlist;

use crate::parser::kicad_scanner::Token;
use crate::parser::kicad_scanner::Scanner;

#[derive(Debug)]
#[derive(PartialEq)]
enum NetListNode {
    Atom(Token),
    List(Vec<NetListNode>)
}

impl NetListNode {
    pub fn atom_as_string(&self) -> anyhow::Result<&String> {
        match self {
            NetListNode::Atom(sym) => {
                match sym {
                    Token::Symbol(str) => {
                        return Ok(str);
                    },
                    Token::Value(str) => {
                        return Ok(str);
                    }
                    Token::LParen | Token::RParen => {
                        bail!("Token was paranthesis!");
                    }

                }
            }
            _ => {
                bail!("NetListNode is not a Atom");
            }
        }
    }

    pub fn as_token(&self) -> anyhow::Result<&Token> {
        match self {
            NetListNode::Atom(sym) => {
                return Ok(sym);
            }
            _ => {
                bail!("NetListNode is not a Atom");
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

    pub fn key(&self) -> anyhow::Result<&Token> {
        let NetListNode::List(list) = self else {
            bail!("NetListNode is not a List");
        };

        let NetListNode::Atom(sym) = &list[0] else {
            bail!("First element on NetListNode::List is not a Atom")
        };

        let Token::Symbol(_) = sym else {
            bail!("Token wasnt symbol")
        };

        return Ok(sym);
    }

    pub fn val(&self) -> anyhow::Result<&String> {
        let NetListNode::List(list) = self else {
            bail!("NetListNode is not a List");
        };

        ensure!( list.len() == 2, format!("Not exactly two, but {}, children of node {:?}, val {:?}", list.len(), self, self));

        let NetListNode::Atom(sym) = &list[1] else {
            bail!("First element on NetListNode::List is not a Atom")
        };

        let Token::Value(val) = sym else {
            bail!("Token wasnt value")
        };
        

        return Ok(val);
    }

    pub fn list(&self) -> anyhow::Result<&[NetListNode]> {
        let NetListNode::List(list) = self else {
            bail!("NetListNode is not a List");
        };

        return list.get(1..).with_context(|| format!("List was too short with length {}", list.len()));

    }

    fn equals_token(&self, tok: &Token) -> bool {
        match self {
            NetListNode::Atom(sym) => {
                if sym == tok {
                    return true;
                };
                return false;
            }
            _  => {
                return false;
            }
        }
    }

    pub fn get_direct_child(&self, child: &Token) -> anyhow::Result<Vec<&NetListNode>> {
        let NetListNode::List(list) = self else {
            bail!("Node wasnt list {:?}", self);
        };

        return Ok(
            list[1..].into_iter()
                .filter_map(|x| match x {
                    NetListNode::List(_) => {
                        match x.key() {
                            Ok(n) => {
                                if n == child {
                                return Some(Ok(x));

                                }
                                return None;
                            },
                            Err(e) => {
                                return Some(Err(e));
                            }
                        }
                    }
                    _ => {
                        return None;
                    }
                })
                .collect::<anyhow::Result<Vec<&NetListNode>>>()
                .context("empty lists in children")?
        )
    }
    
    pub fn get_child(&self, child: &Token) -> anyhow::Result<Vec<&NetListNode>> {
        match self {
            NetListNode::Atom(sym) => {
                if sym == child {
                    return Ok(vec![self]);
                };
                bail!("NetListNode was leaf Symbol but was {:?} instead of {:?}", sym, child);
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

    pub fn get_only_child(&self, child: &Token) -> anyhow::Result<&NetListNode> {
        let vals = self.get_direct_child(child)?;
        ensure!( vals.len() == 1, format!("Not one value child of node {:?}, val {:?}", self, child));

        return Ok(vals.into_iter().next().unwrap())
    }

    pub fn get_maybe_only_child(&self, child: &Token) -> anyhow::Result<Option<&NetListNode>> {
        let vals = self.get_direct_child(child)?;
        match vals.len() {
            0 => {
                return Ok(None);
            }
            1 => {
                return Ok(Some(vals.into_iter().next().unwrap()));
            }
            _ => {
                bail!("More than one value child of node {:?}, val {:?}", self, child);
            }
        }
    }

    pub fn get_direct_child_val(&self, child: &Token) -> anyhow::Result<Vec<&String>> {
        let child_nodes = self.get_direct_child(child)?;
        let list: anyhow::Result<Vec<&String>> = child_nodes
            .into_iter()
            .map(|x| x.val())
            .collect();
        
        return list;
    }

    pub fn get_only_child_val(&self, child: &Token) -> anyhow::Result<&String> {
        let vals = self.get_direct_child_val(child)?;
        ensure!( vals.len() == 1, format!("Not one value child but {} of node {:?}, val {:?}, found {:?}", vals.len(), self, child, vals));

        return Ok(vals.into_iter().next().unwrap())
    }

    pub fn get_maybe_only_child_val(&self, child: &Token) -> anyhow::Result<Option<&String>> {
        let vals = self.get_direct_child_val(child)?;
        match vals.len() {
            0 => {
                return Ok(None);
            }
            1 => {
                return Ok(Some(vals.into_iter().next().unwrap()));
            }
            _ => {
                bail!("More than one value child of node {:?}, val {:?}", self, child);
            }
        }
    }

}

fn print_node(node: &NetListNode, depth: usize) {
    let indent = "  ".repeat(depth);
    match node {
        NetListNode::Atom(tok) => {
            match tok {
                Token::Symbol(a) => {
                    println!("{indent}{:?}", a);
                }
                Token::Value(a) => {
                    println!("{indent}\"{}\"", a);
                }
                _ => {}
            }
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
    let Token::Symbol(str) = sym else {
        bail!("NetListNode passed was not Symbol but {sym:?}")
    };

    ensure!(*str == "comp", "Token passed was not 'comp' but '{str}'");

    let mut comp = netlist::Component::new();
    comp.refdes = node.get_only_child_val(&Token::sym("ref"))
        .with_context(|| format!("error looking for ref child of node {:?}", node))?
        .to_string();
    comp.value = node.get_only_child_val(&Token::sym("value"))
        .with_context(|| format!("error looking for value child of node {:?}", node))?
        .to_string();
    comp.footprint = node.get_maybe_only_child_val(&Token::sym("footprint"))
        .with_context(|| format!("error looking for footprint child of node {:?}", node))?
        .map(|x| x.to_string());

    comp.description = node.get_maybe_only_child_val(&Token::sym("description"))
        .with_context(|| format!("error looking for description child of node {:?}", node))?
        .map(|x| x.to_string());

    comp.sheet = node.get_maybe_only_child(&Token::sym("sheetpath"))
        .with_context(|| format!("error looking for sheethpath child of node {:?}", node))?
        .map(|x| x.get_only_child_val(&Token::sym("names")))
        .transpose()
        .with_context(|| format!("error looking for value child of sheethpath child of node {:?}", node))?
        .map(|x| x.to_string());


    let pins: anyhow::Result<Vec<netlist::Pin>> = node.get_child(&Token::sym("pin"))
        .with_context(|| format!("error looking for pin child of node {:?}", node))?
        .into_iter()
        .map(|x| -> anyhow::Result<netlist::Pin> {
           Ok(netlist::Pin { 
            number: x.get_only_child_val(&Token::sym("num"))?
                .to_string(), 
            name: x.get_maybe_only_child_val(&Token::sym("name"))?
                .map(|x| x.to_string()),
            pin_type: x.get_maybe_only_child_val(&Token::sym("type"))?
                .map(|x| x.to_string()),
            net: None
        })})
        .collect();

    comp.pins = pins?;

    let props = node.get_child(&Token::sym("property"))
        .with_context(|| format!("error looking for property child of node {:?}", node))?
        .into_iter()
        .map(|node| -> anyhow::Result<(String, Option<String>)> {
            let key = node.get_only_child_val(&Token::sym("name"))?;
            let value = node.get_maybe_only_child_val(&Token::sym("value"))?.map(|x| x.to_string());
            Ok((key.to_string(), value))
        })
        .collect::<anyhow::Result<HashMap<String, Option<String>>>>()?;

    comp.properties = props;

    return Ok(comp);
}

fn parse_net(node: &NetListNode, comps: &mut Vec::<netlist::Component>) -> anyhow::Result<netlist::Net> {
    let sym = node.key()?;
    ensure!(*sym == Token::sym("net"), "NetListNode passed was not Symbol::Comp but {sym:?}");

    let code = node.get_only_child_val(&Token::sym("code"))?
        .to_string()
        .parse::<usize>()?;

    let name = node.get_only_child_val(&Token::sym("name"))?
        .to_string();

    let net = netlist::Net {
        code: code,
        name: name
    };


    for node in node.get_child(&Token::sym("node"))? {
        let node_refdes = node.get_only_child_val(&Token::sym("ref"))
            .with_context(|| format!("Couldnt find refdes value of node {:?} on net {}", node, net.name))?
            .to_string();

        let node_pin = node.get_only_child_val(&Token::sym("pin"))
            .with_context(|| format!("Couldnt find pin value of node {:?} on net {}", node, net.name))?
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

fn structurize(syms: &mut &[Token]) -> anyhow::Result<NetListNode> {
    let Token::LParen = syms[0] else { 
        bail!("no left paranthesis in structurize, misaligned")
    };

    *syms = &syms[1..];

    let key;
    match syms[0].clone() {
        Token::LParen => {
            bail!("two left paranthesis after each other in structurize!");
        }
        Token::RParen => {
            bail!("empty node in structurize!");
        }
        x => {
            *syms = &syms[1..];
            key = NetListNode::Atom(x.clone());
        },
    };

    let mut val: Vec<NetListNode> = Vec::new();
    val.push(key);
    loop {
        let elem = match syms[0].clone() {
            Token::LParen => structurize(syms)?,
            Token::RParen => {
                *syms = &syms[1..];
                break;
            }
            x => {
                *syms = &syms[1..];
                NetListNode::Atom(x)
            }
        };
        val.push(elem)
    }

    if val.len() < 2 {
        return Ok(val.pop().unwrap());
    } else {
        return Ok(NetListNode::List(val));
    }

}

pub fn parse_netlist(data: &String) -> anyhow::Result<netlist::Netlist> {
    let scanner = Scanner::new(data);
    let syms: Vec<Token> = scanner.collect::<anyhow::Result<Vec<Token>>>()?;

    let mut slice: &[Token] = &syms;
    let nodetree : NetListNode = structurize(&mut slice)?;

    let mut comps = nodetree.get_child(&Token::sym("comp"))?
        .into_iter()
        .map(parse_component)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let nets = nodetree.get_child(&Token::sym("net"))?
        .into_iter()
        .map(|n| parse_net(n, &mut comps)) 
        .collect::<anyhow::Result<Vec<_>>>()?;

    let netlist = netlist::Netlist {
        components: comps,
        nets: nets
    };

    return Ok(netlist);
}