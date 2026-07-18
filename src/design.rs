use std::collections::HashMap;
use std::fmt;
use std::fmt::format;
use std::fmt::Display;
use anyhow::Context;

use serde::Serialize;

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
    pub fn pin_to_string(&self, pin_id: &PinId) -> String {
        let pin = self.pin(pin_id);
        let comp = self.comp(&pin.comp);
        let mut out = String::new();
        out.push_str(&format!("{}:{}", comp.refdes, pin.number));
        match &pin.name {
            Some(name) => {
                if name.len() > 0 {
                    out.push_str(&format!(" ({})", name));
                }
            }
            None => {}
        }

        match &pin.pin_type {
            Some(pin_type) => {
                if pin_type.len() > 0 {
                    out.push_str(&format!(" (type: {})", pin_type));
                }
            }
            None => {}
        }

        match &pin.net {
            Some(net_id) => {
                let net = self.net(&net_id);
                if net.name.len() > 0 {
                    out.push_str(&format!(" - {}", net.name));
                } else {
                    out.push_str(" - Not Connected");
                }
            }
            None => {
                out.push_str(" - Not Connected");
            }
        }
        
        return out;
    }

    pub fn comp(&self, comp_id: &CompId) -> &Component {
        &self.components[comp_id.0 as usize]
    }

    pub fn pin(&self, pin_id: &PinId) -> &Pin {
        &self.pins[pin_id.0 as usize]
    }

    pub fn net(&self, net_id: &NetId) -> &Net {
        &self.nets[net_id.0 as usize]
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

    fn pin_sort_key(s: &str) -> (&str, u32) {
        let split = s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let (prefix, digits) = s.split_at(split);
        (prefix, digits.parse().unwrap_or(0))
    }

    pub fn comp_details(&self, refdes: &String) -> anyhow::Result<String> {
        let comp = self.component(
            self.component_map
                .get(refdes)
                .with_context(|| format!("Refdes {} not found in component map", refdes))?
        );
        let mut pins = comp.pins.iter()
            .map(|x| (self.pin(x).number.clone(), self.pin_to_string(x))).collect::<Vec<_>>();
        pins.sort_by(|x, y| Self::pin_sort_key(&x.0).cmp(&Self::pin_sort_key(&y.0)));
        let pin_strings = pins.into_iter().map(|x| x.1).collect();
        let json = &ComponentJson {
                refdes: comp.refdes.clone(),
                value: comp.value.clone(),
                description: comp.description.clone(),
                sheet: comp.sheet.clone(),
                footprint: comp.footprint.clone(),
                properties: comp.properties.clone(),
                pins: pin_strings
            };
        return Ok(serde_json::to_string_pretty(json).context("error serializing comp")?);

    }

    /// Components one hop away from `refdes`: for each of its pins (in pin-number
    /// order), the other components sharing that pin's net, grouped by net so a
    /// GND/power rail's huge fanout doesn't drown the small signal nets. Each
    /// group is capped at 25 neighbors (`truncated: true` if more existed).
    pub fn neighbors(&self, refdes: &str) -> anyhow::Result<String> {
        let comp_id = self.component_map
            .get(refdes)
            .with_context(|| format!("component {} not found", refdes))?;
        let comp = self.component(comp_id);

        let mut pins: Vec<&PinId> = comp.pins.iter().collect();
        pins.sort_by(|x, y| {
            Self::pin_sort_key(&self.pin(x).number).cmp(&Self::pin_sort_key(&self.pin(y).number))
        });

        let mut net_groups: Vec<NetGroup> = Vec::new();
        for pin_id in pins {
            let pin = self.pin(pin_id);
            let Some(net_id) = &pin.net else {
                // Unconnected pin — nothing to group.
                continue;
            };
            let net = self.net(net_id);

            let mut other_pins: Vec<&PinId> = net.pins
                .iter()
                .filter(|npid| self.pin(npid).comp.0 != comp_id.0)
                .collect();
            other_pins.sort_by(|a, b| {
                let ac = self.component(&self.pin(a).comp);
                let bc = self.component(&self.pin(b).comp);
                Self::pin_sort_key(&ac.refdes)
                    .cmp(&Self::pin_sort_key(&bc.refdes))
                    .then_with(|| {
                        Self::pin_sort_key(&self.pin(a).number)
                            .cmp(&Self::pin_sort_key(&self.pin(b).number))
                    })
            });

            let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
            let mut neighbors: Vec<NeighborRow> = Vec::new();
            for npid in &other_pins {
                let np = self.pin(npid);
                let nc = self.component(&np.comp);
                if !seen.insert((nc.refdes.clone(), np.number.clone())) {
                    continue;
                }
                neighbors.push(NeighborRow {
                    refdes: nc.refdes.clone(),
                    value: nc.value.clone(),
                    pin: self.pin_name(npid),
                });
            }

            let truncated = neighbors.len() > 25;
            neighbors.truncate(25);

            net_groups.push(NetGroup {
                pin: self.pin_name(pin_id),
                net: net.name.clone(),
                fanout: net.pins.len(),
                truncated,
                neighbors,
            });
        }

        let envelope = NeighborsEnvelope {
            refdes: comp.refdes.clone(),
            value: comp.value.clone(),
            net_groups,
        };
        return Ok(serde_json::to_string_pretty(&envelope)
            .context("error serializing neighbors")?);
    }

    pub fn filter_components(
        &self,
        query: Option<&str>,
        refdes_class: Option<&str>,
        subsystem: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> anyhow::Result<String> {
        // Split the query into lowercased terms; treat empty/whitespace-only as no filter.
        let query_terms: Option<Vec<String>> = query
            .map(|q| q.split_whitespace().map(|t| t.to_lowercase()).collect::<Vec<_>>())
            .filter(|terms: &Vec<String>| !terms.is_empty());
        let refdes_class_lc = refdes_class.map(|c| c.to_lowercase());
        let subsystem_lc = subsystem.map(|s| s.trim_matches('/').to_lowercase());

        let mut matches: Vec<&Component> = self.components
            .iter()
            .filter(|comp| {
                // refdes_class: leading non-digit prefix of refdes, case-insensitive.
                if let Some(class) = &refdes_class_lc {
                    let prefix = comp.refdes
                        .chars()
                        .take_while(|c| !c.is_ascii_digit())
                        .collect::<String>()
                        .to_lowercase();
                    if &prefix != class {
                        return false;
                    }
                }

                // subsystem: case-insensitive substring against sheet, '/' trimmed.
                if let Some(sub) = &subsystem_lc {
                    let sheet_norm = comp.sheet
                        .as_deref()
                        .unwrap_or("")
                        .trim_matches('/')
                        .to_lowercase();
                    if !sheet_norm.contains(sub.as_str()) {
                        return false;
                    }
                }

                // query: every term must appear in the searchable bundle.
                if let Some(terms) = &query_terms {
                    let bundle = comp.search_bundle();
                    if !terms.iter().all(|t| bundle.contains(t.as_str())) {
                        return false;
                    }
                }

                true
            })
            .collect();

        // Natural refdes order: prefix, then numeric suffix (R2 before R10).
        matches.sort_by(|a, b| Self::pin_sort_key(&a.refdes).cmp(&Self::pin_sort_key(&b.refdes)));

        let total = matches.len();
        let rows: Vec<FilterRow> = matches
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|comp| FilterRow {
                refdes: comp.refdes.clone(),
                value: comp.value.clone(),
                description: comp.description.clone(),
                footprint: comp.footprint.clone(),
                sheet: comp.sheet.clone(),
                keywords: comp.properties.get("ki_keywords").cloned().flatten(),
                pin_count: comp.pins.len(),
            })
            .collect();

        let envelope = FilterEnvelope {
            total,
            offset,
            limit,
            returned: rows.len(),
            rows,
        };
        return Ok(serde_json::to_string_pretty(&envelope)
            .context("error serializing filter_components")?);
    }

    /// The front door for locating components: score every component against the
    /// query over the same searchable bundle `filter_components` uses, keep those
    /// above a small floor, and return the top `limit` ranked by confidence.
    ///
    /// Confidence is a max over transparent tiers (see `score_component`); the
    /// `match_reason` carries which tier fired. The reverse-join base-match tier
    /// is the one thing this tool does that `filter_components` cannot: it catches
    /// an over-complete MPN from the datasheet store ("TLA2518IRTERQ1") against a
    /// shorter netlist value ("TLA2518IRTER"), where containment fails.
    pub fn find_components(&self, query: &str, limit: u32) -> anyhow::Result<String> {
        // Blank query is not an error — the front door just yields nothing.
        let query_lower = query.to_lowercase();
        let query_squash = squash(query);
        let terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(f32, String, &Component)> = if query_lower.trim().is_empty() {
            Vec::new()
        } else {
            self.components
                .iter()
                .filter_map(|comp| {
                    score_component(&query_squash, &terms, comp)
                        .map(|(score, reason)| (score, reason, comp))
                })
                .filter(|(score, _, _)| *score >= SCORE_FLOOR)
                .collect()
        };

        // Confidence descending, then break ties by significance. A query that
        // only matches a page/sheet name (e.g. "adc") ties every part on that
        // sheet at the same confidence; prefer higher-pin-count parts (ICs,
        // connectors) over 2-pin passives — a generic agent asking for "adc"
        // wants the ADC, not the 50 decoupling caps that share its sheet. Pin
        // count is a data-driven proxy for "primary part". Natural refdes order
        // (R2 before R10) is the final tie-break for parts of equal size.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.2.pins.len().cmp(&a.2.pins.len()))
                .then_with(|| Self::pin_sort_key(&a.2.refdes).cmp(&Self::pin_sort_key(&b.2.refdes)))
        });

        let candidates: Vec<Candidate> = scored
            .into_iter()
            .take(limit as usize)
            .map(|(score, reason, comp)| Candidate {
                row: FilterRow {
                    refdes: comp.refdes.clone(),
                    value: comp.value.clone(),
                    description: comp.description.clone(),
                    footprint: comp.footprint.clone(),
                    sheet: comp.sheet.clone(),
                    keywords: comp.properties.get("ki_keywords").cloned().flatten(),
                    pin_count: comp.pins.len(),
                },
                confidence: score,
                match_reason: reason,
            })
            .collect();

        let envelope = FindEnvelope {
            query: query.to_string(),
            returned: candidates.len(),
            candidates,
        };
        return Ok(serde_json::to_string_pretty(&envelope)
            .context("error serializing find_components")?);
    }

    /// The `sheet` of every component that owns a pin on this net. A net has no
    /// sheet of its own; its subsystem is derived from what it connects. Used by
    /// `filter_nets`' subsystem predicate. Duplicates are not deduped — the callers
    /// only ask `any(...)`.
    fn net_component_sheets<'a>(&'a self, net: &'a Net) -> impl Iterator<Item = &'a str> {
        net.pins
            .iter()
            .filter_map(move |pid| self.component(&self.pin(pid).comp).sheet.as_deref())
    }

    /// Net-side counterpart of `filter_components`: deterministic, exhaustive,
    /// no scoring. Filter by name substring and/or subsystem (AND-combined,
    /// case-insensitive), sort, paginate, and serialize a compact envelope.
    pub fn filter_nets(
        &self,
        name: Option<&str>,
        subsystem: Option<&str>,
        sort_by_fanout: bool,
        limit: u32,
        offset: u32,
    ) -> anyhow::Result<String> {
        // Empty/whitespace-only name is treated as no filter.
        let name_lc = name
            .map(|n| n.to_lowercase())
            .filter(|n| !n.trim().is_empty());
        let subsystem_lc = subsystem.map(|s| s.trim_matches('/').to_lowercase());

        let mut matches: Vec<&Net> = self.nets
            .iter()
            .filter(|net| {
                // name: case-insensitive substring against the net name.
                if let Some(n) = &name_lc {
                    if !net.name.to_lowercase().contains(n.as_str()) {
                        return false;
                    }
                }

                // subsystem: any connected component's sheet contains the filter,
                // '/' trimmed on both sides (same normalization as filter_components).
                if let Some(sub) = &subsystem_lc {
                    let hit = self.net_component_sheets(net).any(|sheet| {
                        sheet.trim_matches('/').to_lowercase().contains(sub.as_str())
                    });
                    if !hit {
                        return false;
                    }
                }

                true
            })
            .collect();

        // Default (sort_by_fanout): fanout descending, tie-break net name ascending.
        // Otherwise: alphabetical by net name.
        if sort_by_fanout {
            matches.sort_by(|a, b| {
                b.pins.len().cmp(&a.pins.len()).then_with(|| a.name.cmp(&b.name))
            });
        } else {
            matches.sort_by(|a, b| a.name.cmp(&b.name));
        }

        let total = matches.len();
        let rows: Vec<NetRow> = matches
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|net| NetRow {
                name: net.name.clone(),
                code: net.code,
                fanout: net.pins.len(),
                pin_types: net.pin_types.clone(),
            })
            .collect();

        let envelope = NetEnvelope {
            total,
            offset,
            limit,
            returned: rows.len(),
            rows,
        };
        return Ok(serde_json::to_string_pretty(&envelope)
            .context("error serializing filter_nets")?);
    }

    /// Group components by their schematic sheet ("subsystem") and count them.
    /// Components with no sheet (or an empty one) land in an `(unassigned)`
    /// bucket with a null path. Sheets in this design are single-level, so a
    /// flat grouping by the raw sheet string is correct — no tree needed.
    pub fn list_subsystems(&self) -> anyhow::Result<String> {
        let mut counts: HashMap<Option<String>, usize> = HashMap::new();
        for comp in &self.components {
            let key = comp.sheet
                .as_ref()
                .filter(|s| !s.is_empty())
                .cloned();
            *counts.entry(key).or_insert(0) += 1;
        }

        let mut subsystems: Vec<SubsystemRow> = counts
            .into_iter()
            .map(|(sheet, component_count)| match sheet {
                Some(path) => {
                    let name = path.trim_matches('/').to_string();
                    SubsystemRow { path: Some(path), name, component_count }
                }
                None => SubsystemRow {
                    path: None,
                    name: "(unassigned)".to_string(),
                    component_count,
                },
            })
            .collect();

        subsystems.sort_by(|a, b| {
            b.component_count.cmp(&a.component_count).then_with(|| a.name.cmp(&b.name))
        });

        let envelope = SubsystemEnvelope {
            total_components: self.components.len(),
            subsystem_count: subsystems.len(),
            subsystems,
        };
        return Ok(serde_json::to_string_pretty(&envelope)
            .context("error serializing list_subsystems")?);
    }

    /// Estimate the probability that `net` is a power/ground rail, as a
    /// transparent weighted sum of three signals (power-typed pin fraction,
    /// name pattern, decoupling-cap fraction) plus a high-fanout boost.
    /// Returns the score in [0,1] and human-readable evidence strings for the
    /// signals that fired. Public and reusable — `walk` uses this to decide
    /// when to stop at a rail instead of enumerating it.
    pub fn rail_score(&self, net: &Net) -> (f32, Vec<String>) {
        let fanout = net.pins.len();
        if fanout == 0 {
            return (0.0, Vec::new());
        }

        let power_pins: i32 = net.pin_types.get("power_in").copied().unwrap_or(0)
            + net.pin_types.get("power_out").copied().unwrap_or(0);
        let power_frac = power_pins as f32 / fanout as f32;

        let name_match = is_power_name(&net.name);

        let cap_pins = net.pins
            .iter()
            .filter(|pid| {
                let refdes = &self.component(&self.pin(pid).comp).refdes;
                let prefix: String = refdes
                    .chars()
                    .take_while(|c| !c.is_ascii_digit())
                    .collect::<String>()
                    .to_uppercase();
                prefix == "C"
            })
            .count();
        let cap_frac = cap_pins as f32 / fanout as f32;

        let fanout_boost = if fanout > 20 && (name_match || cap_frac > 0.3) {
            RAIL_FANOUT_BOOST
        } else {
            0.0
        };

        let score = (RAIL_WEIGHT_POWER_FRAC * power_frac
            + RAIL_WEIGHT_NAME_MATCH * (name_match as i32 as f32)
            + RAIL_WEIGHT_CAP_FRAC * cap_frac
            + fanout_boost)
            .clamp(0.0, 1.0);

        let mut evidence: Vec<String> = Vec::new();
        if power_frac > 0.1 {
            evidence.push(format!("{:.0}% power pins", power_frac * 100.0));
        }
        if name_match {
            evidence.push("name matches power pattern".to_string());
        }
        if cap_frac > 0.1 {
            evidence.push(format!("{:.0}% capacitors", cap_frac * 100.0));
        }
        if fanout_boost > 0.0 {
            evidence.push(format!("high fanout ({fanout} pins)"));
        }

        (score, evidence)
    }

    /// Zero-knowledge orientation summary: counts, refdes-class histogram,
    /// detected power rails (via `rail_score`), connectors, subsystems, and
    /// the highest-fanout nets. Lists are capped — this is an overview, not
    /// an exhaustive dump.
    pub fn design_overview(&self) -> anyhow::Result<String> {
        let counts = OverviewCounts {
            components: self.components.len(),
            nets: self.nets.len(),
            pins: self.pins.len(),
        };

        // refdes_classes: histogram over leading-alpha class, count desc then class asc.
        let mut class_counts: HashMap<String, usize> = HashMap::new();
        for comp in &self.components {
            let class = comp.refdes
                .chars()
                .take_while(|c| !c.is_ascii_digit())
                .collect::<String>()
                .to_uppercase();
            *class_counts.entry(class).or_insert(0) += 1;
        }
        let mut refdes_classes: Vec<RefdesClassRow> = class_counts
            .into_iter()
            .map(|(class, count)| RefdesClassRow { class, count })
            .collect();
        refdes_classes.sort_by(|a, b| {
            b.count.cmp(&a.count).then_with(|| a.class.cmp(&b.class))
        });

        // rails: every net scoring >= 0.5, sorted score desc then fanout desc, capped 25.
        let mut rails: Vec<RailRow> = self.nets
            .iter()
            .filter_map(|net| {
                let (score, evidence) = self.rail_score(net);
                if score >= 0.5 {
                    Some(RailRow {
                        net: net.name.clone(),
                        fanout: net.pins.len(),
                        score: (score * 100.0).round() / 100.0,
                        evidence,
                    })
                } else {
                    None
                }
            })
            .collect();
        rails.sort_by(|a, b| {
            b.score.partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.fanout.cmp(&a.fanout))
        });
        rails.truncate(25);

        // connectors: refdes class J or P, natural refdes order, capped 50.
        let mut connectors: Vec<&Component> = self.components
            .iter()
            .filter(|comp| {
                let class = comp.refdes
                    .chars()
                    .take_while(|c| !c.is_ascii_digit())
                    .collect::<String>()
                    .to_uppercase();
                class == "J" || class == "P"
            })
            .collect();
        connectors.sort_by(|a, b| Self::pin_sort_key(&a.refdes).cmp(&Self::pin_sort_key(&b.refdes)));
        let connectors: Vec<ConnectorRow> = connectors
            .into_iter()
            .take(50)
            .map(|comp| ConnectorRow {
                refdes: comp.refdes.clone(),
                value: comp.value.clone(),
                pin_count: comp.pins.len(),
            })
            .collect();

        // subsystems: group by sheet, top 15 by count.
        let mut sheet_counts: HashMap<Option<String>, usize> = HashMap::new();
        for comp in &self.components {
            let key = comp.sheet
                .as_ref()
                .filter(|s| !s.is_empty())
                .cloned();
            *sheet_counts.entry(key).or_insert(0) += 1;
        }
        let mut subsystems: Vec<SubsystemSummaryRow> = sheet_counts
            .into_iter()
            .map(|(sheet, component_count)| {
                let name = match &sheet {
                    Some(path) => path.trim_matches('/').to_string(),
                    None => "(unassigned)".to_string(),
                };
                SubsystemSummaryRow { name, component_count }
            })
            .collect();
        subsystems.sort_by(|a, b| {
            b.component_count.cmp(&a.component_count).then_with(|| a.name.cmp(&b.name))
        });
        subsystems.truncate(15);

        // top_nets_by_fanout: top 15 by fanout.
        let mut nets_by_fanout: Vec<&Net> = self.nets.iter().collect();
        nets_by_fanout.sort_by(|a, b| {
            b.pins.len().cmp(&a.pins.len()).then_with(|| a.name.cmp(&b.name))
        });
        let top_nets_by_fanout: Vec<NetFanoutRow> = nets_by_fanout
            .into_iter()
            .take(15)
            .map(|net| NetFanoutRow { net: net.name.clone(), fanout: net.pins.len() })
            .collect();

        let envelope = OverviewEnvelope {
            counts,
            refdes_classes,
            rails,
            connectors,
            subsystems,
            top_nets_by_fanout,
        };
        return Ok(serde_json::to_string_pretty(&envelope)
            .context("error serializing design_overview")?);
    }

    pub fn from_netlist(netlist: netlist::Netlist) -> anyhow::Result<Design> {
        let mut nets: Vec<Net> = Vec::new();
        let mut net_map: HashMap<String, NetId> = HashMap::new();
        for (i, netlist_net) in netlist.nets.into_iter().enumerate() {
            let net = Net {
                id: NetId(i),
                code: netlist_net.code,
                name: netlist_net.name,
                pins: Vec::new(),
                pin_types: HashMap::new()
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
                description: netlist_comp.description,
                sheet: netlist_comp.sheet,
                properties: netlist_comp.properties,
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
                    pin_type: netlist_pin.pin_type,
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

        for net in &mut nets {
            let pin_types = net.pins
                .iter()
                .map(|x| &pins[x.0 as usize])
                .map(|x| &x.pin_type)
                .flatten()
                .fold(HashMap::new(), |mut acc, x| {
                    *acc.entry(x.clone()).or_insert(0) += 1;
                    acc
                });
                net.pin_types = pin_types;

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

#[derive(Debug, Serialize)]
pub struct CompId(usize);

#[derive(Debug, Serialize)]
pub struct ComponentJson {
    refdes: String,
    value: String,
    footprint: Option<String>,
    description: Option<String>,
    sheet: Option<String>,
    properties: HashMap<String, Option<String>>,
    pins: Vec<String>
}

#[derive(Debug, Serialize)]
struct FilterRow {
    refdes: String,
    value: String,
    description: Option<String>,
    footprint: Option<String>,
    sheet: Option<String>,
    keywords: Option<String>,
    pin_count: usize,
}

#[derive(Debug, Serialize)]
struct FilterEnvelope {
    total: usize,
    offset: u32,
    limit: u32,
    returned: usize,
    rows: Vec<FilterRow>,
}

/// One neighbor in a `neighbors` net group: the other component on the shared
/// net, plus which of its pins carries it.
#[derive(Debug, Serialize)]
struct NeighborRow {
    refdes: String,
    value: String,
    pin: String,
}

/// One net shared between the queried component and others: the queried
/// component's own pin on it, the net's identity/fanout, and the (capped)
/// neighbor list.
#[derive(Debug, Serialize)]
struct NetGroup {
    pin: String,
    net: String,
    fanout: usize,
    truncated: bool,
    neighbors: Vec<NeighborRow>,
}

#[derive(Debug, Serialize)]
struct NeighborsEnvelope {
    refdes: String,
    value: String,
    net_groups: Vec<NetGroup>,
}

/// One filter_nets hit: net identity, fanout, and the raw per-type pin histogram.
/// Member pins are deliberately not expanded — that is get_net's job.
#[derive(Debug, Serialize)]
struct NetRow {
    name: String,
    code: usize,
    fanout: usize,
    pin_types: HashMap<String, i32>,
}

#[derive(Debug, Serialize)]
struct NetEnvelope {
    total: usize,
    offset: u32,
    limit: u32,
    returned: usize,
    rows: Vec<NetRow>,
}

/// One subsystem bucket from `list_subsystems`: the raw sheet path (null for
/// unassigned), a display name, and how many components sit on it.
#[derive(Debug, Serialize)]
struct SubsystemRow {
    path: Option<String>,
    name: String,
    component_count: usize,
}

#[derive(Debug, Serialize)]
struct SubsystemEnvelope {
    total_components: usize,
    subsystem_count: usize,
    subsystems: Vec<SubsystemRow>,
}

#[derive(Debug, Serialize)]
struct OverviewCounts {
    components: usize,
    nets: usize,
    pins: usize,
}

/// One refdes-class bucket in `design_overview` (e.g. "C" -> 220 parts).
#[derive(Debug, Serialize)]
struct RefdesClassRow {
    class: String,
    count: usize,
}

/// One detected power/ground rail in `design_overview`, from `Design::rail_score`.
#[derive(Debug, Serialize)]
struct RailRow {
    net: String,
    fanout: usize,
    score: f32,
    evidence: Vec<String>,
}

/// One connector (refdes class J/P) in `design_overview`.
#[derive(Debug, Serialize)]
struct ConnectorRow {
    refdes: String,
    value: String,
    pin_count: usize,
}

/// One subsystem bucket in `design_overview` (compact form of `SubsystemRow`,
/// no raw sheet path — that detail belongs to `list_subsystems`).
#[derive(Debug, Serialize)]
struct SubsystemSummaryRow {
    name: String,
    component_count: usize,
}

/// One net in the `design_overview` fanout leaderboard.
#[derive(Debug, Serialize)]
struct NetFanoutRow {
    net: String,
    fanout: usize,
}

#[derive(Debug, Serialize)]
struct OverviewEnvelope {
    counts: OverviewCounts,
    refdes_classes: Vec<RefdesClassRow>,
    rails: Vec<RailRow>,
    connectors: Vec<ConnectorRow>,
    subsystems: Vec<SubsystemSummaryRow>,
    top_nets_by_fanout: Vec<NetFanoutRow>,
}

/// One ranked find_components hit: the same compact row as filter_components plus
/// the two ranking fields.
#[derive(Debug, Serialize)]
struct Candidate {
    #[serde(flatten)]
    row: FilterRow,
    confidence: f32,
    match_reason: String,
}

#[derive(Debug, Serialize)]
struct FindEnvelope {
    query: String,
    returned: usize,
    candidates: Vec<Candidate>,
}

/// Drop candidates below this so pure noise doesn't surface.
const SCORE_FLOOR: f32 = 0.15;

// Hand-tuned priors for `Design::rail_score`. Kept as named consts so they
// are easy to retune without hunting through the scoring logic.
const RAIL_WEIGHT_POWER_FRAC: f32 = 0.45;
const RAIL_WEIGHT_NAME_MATCH: f32 = 0.30;
const RAIL_WEIGHT_CAP_FRAC: f32 = 0.25;
const RAIL_FANOUT_BOOST: f32 = 0.15;

/// Case-insensitive heuristic for "does this net name look like a power/ground
/// rail?" Checks the segment after the last '/' against a set of common rail
/// names, a leading +/- sign, or a supply-voltage token like "3v3"/"1v8".
fn is_power_name(name: &str) -> bool {
    const RAIL_NAMES: &[&str] = &[
        "gnd", "gnda", "agnd", "dgnd", "pgnd", "vss", "vssa", "vcc", "vdd",
        "vbat", "vbus", "vee", "vin",
    ];

    let segment = name.rsplit('/').next().unwrap_or(name).trim();
    let lower = segment.to_lowercase();

    if RAIL_NAMES.contains(&lower.as_str()) {
        return true;
    }
    if segment.starts_with('+') || segment.starts_with('-') {
        return true;
    }

    // Supply-voltage token: a digit immediately adjacent to a 'v', e.g.
    // "3v3", "5v", "1v8", "3.3v".
    let bytes = lower.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'v' {
            let prev_digit = i > 0 && (bytes[i - 1].is_ascii_digit());
            let next_digit = i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit();
            if prev_digit || next_digit {
                return true;
            }
        }
    }

    false
}

/// Normalize for comparison: lowercase, then keep only alphanumerics — strips
/// spaces/dashes/dots/slashes so "ADS-1115" == "ads1115".
fn squash(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Bidirectional prefix match on already-squashed strings, requiring at least
/// `min` shared chars. Either string being a prefix of the other counts — this
/// is the reverse-join tier that catches partial *and* over-complete MPNs.
fn base_match(a: &str, b: &str, min: usize) -> bool {
    if a.len().min(b.len()) < min {
        return false;
    }
    a.starts_with(b) || b.starts_with(a)
}

/// Score one component against a query as the highest-scoring signal that fires.
/// Tiers are checked in descending-confidence order, so the first hit is the max;
/// the returned reason names that tier. `None` means nothing fired.
fn score_component(query_squash: &str, terms: &[&str], comp: &Component) -> Option<(f32, String)> {
    let value_squash = squash(&comp.value);
    let refdes_squash = squash(&comp.refdes);

    // Tier 1: exact identity.
    if !query_squash.is_empty() && (query_squash == value_squash || query_squash == refdes_squash) {
        let which = if query_squash == value_squash { "value" } else { "refdes" };
        return Some((1.0, format!("exact {which}")));
    }

    // Tier 2: reverse-join base-match against value (min 4 shared chars).
    if base_match(&value_squash, query_squash, 4) {
        let reason = if value_squash.starts_with(query_squash) {
            "value base-match (field starts with query)"
        } else {
            "value base-match (query starts with field)"
        };
        return Some((0.85, reason.to_string()));
    }

    // Tier 3: value substring (not prefix-anchored — those fell into tier 2).
    if !query_squash.is_empty() && value_squash.contains(query_squash) {
        return Some((0.65, "value substring".to_string()));
    }

    // Tiers 4 & 5: whitespace terms against the searchable bundle.
    if !terms.is_empty() {
        let bundle = comp.search_bundle();
        let matched = terms.iter().filter(|t| bundle.contains(**t)).count();
        let n = terms.len();
        if matched == n {
            return Some((0.55, "all terms matched".to_string()));
        } else if matched > 0 {
            let score = 0.20 + 0.25 * (matched as f32 / n as f32);
            return Some((score, format!("matched {matched}/{n} terms")));
        }
    }

    None
}

#[derive(Debug)]
pub struct Component {
    id: CompId,
    refdes: String,
    value: String,
    footprint: Option<String>,
    description: Option<String>,
    sheet: Option<String>,
    properties: HashMap<String, Option<String>>,
    pins: Vec<PinId>
}

impl Component {
    /// The component's searchable text: its own local fields lowercased and
    /// space-joined. Shared by filter_components and find_components so the two
    /// tools agree on what is searchable. Property *values* are included; keys
    /// carry no signal.
    fn search_bundle(&self) -> String {
        let mut bundle = String::new();
        bundle.push_str(&self.refdes.to_lowercase());
        bundle.push(' ');
        bundle.push_str(&self.value.to_lowercase());
        bundle.push(' ');
        if let Some(d) = &self.description {
            bundle.push_str(&d.to_lowercase());
            bundle.push(' ');
        }
        if let Some(f) = &self.footprint {
            bundle.push_str(&f.to_lowercase());
            bundle.push(' ');
        }
        if let Some(s) = &self.sheet {
            bundle.push_str(&s.to_lowercase());
            bundle.push(' ');
        }
        for val in self.properties.values().flatten() {
            bundle.push_str(&val.to_lowercase());
            bundle.push(' ');
        }
        bundle
    }
}

#[derive(Debug, Serialize)]
pub struct PinId(usize);

#[derive(Debug)]
pub struct Pin {
    pub id: PinId,
    pub comp: CompId,
    pub number: String,
    pub name: Option<String>,
    pub pin_type: Option<String>,
    pub net: Option<NetId>
}

#[derive(Debug)]
pub struct NetId(usize);

#[derive(Debug)]
pub struct Net {
    pub id: NetId,
    pub code: usize,
    pub name: String,
    pub pins: Vec<PinId>,
    pub pin_types: HashMap<String, i32>
}