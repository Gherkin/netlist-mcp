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

    pub fn list(&self) -> anyhow::Result<&[NetListNode]> {
        let NetListNode::List(list) = self else {
            bail!("NetListNode is not a List");
        };

        return list.get(1..).with_context(|| format!("List was too short with length {}", list.len()));

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

    pub fn get_child_val(&self, child: &Token) -> anyhow::Result<Vec<&String>> {
        let child_nodes = self.get_child(child)?;
        let list: anyhow::Result<Vec<&String>> = child_nodes
            .into_iter()
            .map(|x| -> anyhow::Result<&String> {
                let items = x.list()?;
                let first = items.get(0).context("child node had no value")?;
                let sym = first.as_token()?;
                let Token::Value(_) = sym else {
                    bail!("Token wasnt Value");
                };
                return first.atom_as_string();
            })
            .collect();
        
        return list;
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
    comp.refdes = node.get_child_val(&Token::Symbol("ref".to_string()))?.get(0)
        .context("Comp Ref list was empty")?
        .to_string();
    comp.value = node.get_child_val(&Token::Symbol("value".to_string()))?.get(0)
        .context("Comp Value list was empty")?
        .to_string();
    comp.footprint = node.get_child_val(&Token::Symbol("footprint".to_string()))?.get(0)
        .map(|x| x.to_string());

    let pins: anyhow::Result<Vec<netlist::Pin>> = node.get_child(&Token::Symbol("pin".to_string()))?
        .into_iter()
        .map(|x| -> anyhow::Result<netlist::Pin> {
           Ok(netlist::Pin { 
            number: x.get_child_val(&Token::Symbol("num".to_string()))?
                .get(0)
                .with_context(|| format!("Pin {x:?} had no number"))?
                .to_string(), 
            name: x.get_child_val(&Token::Symbol("name".to_string()))?
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
    ensure!(*sym == Token::Symbol("net".to_string()), "NetListNode passed was not Symbol::Comp but {sym:?}");

    let code = node.get_child_val(&Token::Symbol("code".to_string()))?
        .get(0)
        .context("Net Code list was empty")?
        .to_string()
        .parse::<usize>()?;

    let name = node.get_child_val(&Token::Symbol("name".to_string()))?
        .get(0)
        .context("Net Name list was empty")?
        .to_string();

    let net = netlist::Net {
        code: code,
        name: name
    };


    for node in node.get_child(&Token::Symbol("node".to_string()))? {
        let node_refdes = node.get_child_val(&Token::Symbol("ref".to_string()))
            .with_context(|| format!("Couldnt find refdes value of node {:?} on net {}", node, net.name))?
            .get(0)
            .with_context(|| format!("Net {} had a node with empty ref", net.name))?
            .to_string();

        let node_pin = node.get_child_val(&Token::Symbol("pin".to_string()))
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

    let mut comps = nodetree.get_child(&Token::Symbol("comp".to_string()))?
        .into_iter()
        .map(parse_component)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let nets = nodetree.get_child(&Token::Symbol("net".to_string()))?
        .into_iter()
        .map(|n| parse_net(n, &mut comps)) 
        .collect::<anyhow::Result<Vec<_>>>()?;

    let netlist = netlist::Netlist {
        components: comps,
        nets: nets
    };

    return Ok(netlist);
}