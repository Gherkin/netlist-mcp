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

    pub fn component(&self, comp_id: &CompId) -> &Component {
        &self.components[comp_id.0 as usize]
    }

    fn pin_sort_key(s: &str) -> (&str, u32) {
        let split = s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let (prefix, digits) = s.split_at(split);
        (prefix, digits.parse().unwrap_or(0))
    }

    /// Full detail on one component: identity, keywords (from `ki_keywords`),
    /// footprint, subsystem, the full property map, and its pins — sorted by
    /// `pin_sort_key` and paginated — each with name/type/net (net is the net
    /// name, or null if unconnected).
    pub fn comp_details(&self, refdes: &str, limit: u32, offset: u32) -> anyhow::Result<String> {
        let comp = self.component(
            self.component_map
                .get(refdes)
                .with_context(|| format!("Refdes {} not found in component map", refdes))?
        );

        let mut pin_ids: Vec<&PinId> = comp.pins.iter().collect();
        pin_ids.sort_by(|a, b| {
            Self::pin_sort_key(&self.pin(a).number).cmp(&Self::pin_sort_key(&self.pin(b).number))
        });

        let pin_count = pin_ids.len();
        let pins: Vec<ComponentPinRow> = pin_ids
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|pid| {
                let pin = self.pin(pid);
                ComponentPinRow {
                    pin: self.pin_name(pid),
                    name: pin.name.clone(),
                    pin_type: pin.pin_type.clone(),
                    net: pin.net.as_ref().map(|nid| self.net(nid).name.clone()),
                }
            })
            .collect();

        let envelope = ComponentDetail {
            refdes: comp.refdes.clone(),
            value: comp.value.clone(),
            description: comp.description.clone(),
            keywords: comp.properties.get("ki_keywords").cloned().flatten(),
            footprint: comp.footprint.clone(),
            sheet: comp.sheet.clone(),
            properties: comp.properties.clone(),
            pin_count,
            offset,
            limit,
            returned: pins.len(),
            pins,
        };
        return Ok(serde_json::to_string_pretty(&envelope).context("error serializing comp_details")?);
    }

    /// Full detail on one net: identity/fanout, rail-score with evidence (via
    /// `rail_score`), the pin-type histogram, the distinct connected subsystems
    /// (via `net_component_sheets`), and paginated member pins — sorted by
    /// owning component's refdes then pin number. Accepts a net name (via
    /// `net_map`) or a net code (parsed from the string).
    pub fn net_details(&self, net: &str, limit: u32, offset: u32) -> anyhow::Result<String> {
        let net_ref: &Net = self.net_map.get(net)
            .map(|id| self.net(id))
            .or_else(|| {
                net.parse::<usize>().ok()
                    .and_then(|code| self.nets.iter().find(|n| n.code == code))
            })
            .with_context(|| format!("no net named or coded '{}'", net))?;

        let (score, evidence) = self.rail_score(net_ref);

        let mut subsystems: Vec<String> = self.net_component_sheets(net_ref)
            .map(|s| s.to_string())
            .collect();
        subsystems.sort();
        subsystems.dedup();

        let mut pin_ids: Vec<&PinId> = net_ref.pins.iter().collect();
        pin_ids.sort_by(|a, b| {
            let ac = self.component(&self.pin(a).comp);
            let bc = self.component(&self.pin(b).comp);
            Self::pin_sort_key(&ac.refdes)
                .cmp(&Self::pin_sort_key(&bc.refdes))
                .then_with(|| {
                    Self::pin_sort_key(&self.pin(a).number)
                        .cmp(&Self::pin_sort_key(&self.pin(b).number))
                })
        });

        let fanout = pin_ids.len();
        let members: Vec<NetMemberRow> = pin_ids
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|pid| {
                let pin = self.pin(pid);
                let comp = self.component(&pin.comp);
                NetMemberRow {
                    pin: self.pin_name(pid),
                    refdes: comp.refdes.clone(),
                    value: comp.value.clone(),
                    pin_name: pin.name.clone(),
                    pin_type: pin.pin_type.clone(),
                }
            })
            .collect();

        let role = self.net_role(net_ref);
        let hierarchy = Self::net_hierarchy(&net_ref.name);

        let envelope = NetDetail {
            net: net_ref.name.clone(),
            code: net_ref.code,
            fanout,
            rail_score: (score * 100.0).round() / 100.0,
            rail_evidence: evidence,
            pin_types: net_ref.pin_types.clone(),
            subsystems,
            role,
            hierarchy,
            offset,
            limit,
            returned: members.len(),
            members,
        };
        return Ok(serde_json::to_string_pretty(&envelope).context("error serializing net_details")?);
    }

    /// Full detail on one pin (REFDES:PIN): its name/function, electrical type,
    /// owning component, and the net it sits on (name/code/fanout/rail_score),
    /// or null if the pin is unconnected.
    pub fn pin_details(&self, pin_name: &str) -> anyhow::Result<String> {
        let pin_id = self.pin_map
            .get(pin_name)
            .with_context(|| format!("no pin named '{}'", pin_name))?;
        let pin = self.pin(pin_id);
        let comp = self.component(&pin.comp);

        let net = pin.net.as_ref().map(|net_id| {
            let net = self.net(net_id);
            let (score, _evidence) = self.rail_score(net);
            PinNetInfo {
                name: net.name.clone(),
                code: net.code,
                fanout: net.pins.len(),
                rail_score: (score * 100.0).round() / 100.0,
            }
        });

        let envelope = PinDetail {
            pin: self.pin_name(pin_id),
            name: pin.name.clone(),
            pin_type: pin.pin_type.clone(),
            component: PinComponentInfo {
                refdes: comp.refdes.clone(),
                value: comp.value.clone(),
                description: comp.description.clone(),
                sheet: comp.sheet.clone(),
            },
            net,
        };
        return Ok(serde_json::to_string_pretty(&envelope).context("error serializing pin_details")?);
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
            .map(|net| {
                let hierarchy = Self::net_hierarchy(&net.name);
                NetRow {
                    name: net.name.clone(),
                    code: net.code,
                    fanout: net.pins.len(),
                    pin_types: net.pin_types.clone(),
                    sheet_path: hierarchy.sheet_path,
                    depth: hierarchy.depth,
                }
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

    /// Per-net counts of owning-component classes, split into IC ("U"),
    /// connector ("J"/"P"), passive ("R"/"L"/"C"/"FB"), and everything else.
    /// Shared by `net_role` and `audit` so both agree on what "IC pin",
    /// "connector pin", and "passive-only" mean.
    fn net_class_counts(&self, net: &Net) -> NetClassCounts {
        let mut counts = NetClassCounts::default();
        for pid in &net.pins {
            let comp = self.component(&self.pin(pid).comp);
            let class = Self::refdes_class(&comp.refdes);
            if class == IC_CLASS {
                counts.ic += 1;
            } else if CONNECTOR_CLASSES.contains(&class.as_str()) {
                counts.connector += 1;
            } else if PASSIVE_CLASSES.contains(&class.as_str()) {
                counts.passive += 1;
            } else {
                counts.other += 1;
            }
        }
        counts
    }

    /// Factual classification of a net's role in the connectivity graph,
    /// derived purely from `net.pin_types` presence and owning-component
    /// classes (see `net_class_counts`). This makes NO judgment about
    /// whether a pattern is a defect — e.g. `has_power_in && !has_source` is
    /// common and correct for a net whose source lives off-net (a regulator
    /// output net, a jumper-fed rail, etc.). Reused by `get_net` (embedded as
    /// `role`) and `audit` (the bucketing predicates).
    pub fn net_role(&self, net: &Net) -> NetRole {
        let has_source = net.pin_types.get("power_out").copied().unwrap_or(0) > 0;
        let has_driver = DRIVER_PIN_TYPES
            .iter()
            .any(|t| net.pin_types.get(*t).copied().unwrap_or(0) > 0);
        let has_power_in = net.pin_types.get("power_in").copied().unwrap_or(0) > 0;
        let has_input = net.pin_types.get("input").copied().unwrap_or(0) > 0;

        let counts = self.net_class_counts(net);
        let fanout = net.pins.len();
        let passive_only = fanout > 0 && counts.passive == fanout;

        NetRole {
            has_source,
            has_driver,
            has_power_in,
            has_input,
            ic_pin_count: counts.ic,
            passive_only,
        }
    }

    /// Pure, structural decomposition of a net name by its '/'-separated
    /// hierarchy. Splits on '/' and drops empty segments (so a leading '/'
    /// and any repeated slashes are both handled cleanly). Does not infer
    /// intended connectivity, scope, or cross-sheet bridging — only reports
    /// the name's literal segment structure. Reused by `get_net` (embedded
    /// as `hierarchy`) and `filter_nets` (compact `sheet_path`/`depth`
    /// fields on each row).
    pub fn net_hierarchy(name: &str) -> NetHierarchy {
        let rooted = name.starts_with('/');
        let segments: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();

        let local_name = segments.last().copied().unwrap_or(name).to_string();
        let depth = segments.len().saturating_sub(1);
        let sheet_path = if segments.len() > 1 {
            Some(format!("/{}", segments[..segments.len() - 1].join("/")))
        } else {
            None
        };
        let scope_hint = if rooted { "hierarchical" } else { "flat" };

        NetHierarchy { rooted, local_name, sheet_path, depth, scope_hint }
    }

    /// Scan every net and bucket it into FACTUAL, non-exclusive categories
    /// describing observed graph patterns worth a human's attention — this
    /// never asserts a defect or infers intent, only reports what the graph
    /// looks like:
    /// - `unpowered_power_in`: has a `power_in` pin, no `power_out` source
    ///   anywhere on the net.
    /// - `undriven_input`: has an `input` pin, no driver-typed pin on the net.
    /// - `single_ic_pin`: touches exactly one IC pin, and every other pin on
    ///   the net belongs to a passive component.
    /// - `stub`: a single-pin (or unconnected) net, or a multi-pin net with
    ///   no IC and no connector pin (passive/other parts only).
    ///
    /// Each bucket is sorted by fanout descending (name ascending on ties),
    /// reports the true `count`, and returns up to `limit` rows — this keeps
    /// hundreds of stub/TP nets from drowning the response.
    pub fn audit(&self, limit: u32) -> anyhow::Result<String> {
        let mut unpowered_power_in: Vec<AuditNetRow> = Vec::new();
        let mut undriven_input: Vec<AuditNetRow> = Vec::new();
        let mut single_ic_pin: Vec<AuditNetRow> = Vec::new();
        let mut stub: Vec<AuditNetRow> = Vec::new();

        for net in &self.nets {
            let fanout = net.pins.len();
            let role = self.net_role(net);
            let counts = self.net_class_counts(net);

            if role.has_power_in && !role.has_source {
                let power_in_count = net.pin_types.get("power_in").copied().unwrap_or(0);
                unpowered_power_in.push(AuditNetRow {
                    net: net.name.clone(),
                    code: net.code,
                    fanout,
                    note: format!("{power_in_count} power_in pin(s), no power_out source"),
                });
            }

            if role.has_input && !role.has_driver {
                let input_count = net.pin_types.get("input").copied().unwrap_or(0);
                undriven_input.push(AuditNetRow {
                    net: net.name.clone(),
                    code: net.code,
                    fanout,
                    note: format!("{input_count} input pin(s), no driver"),
                });
            }

            // Exactly one IC pin, and every non-IC pin on the net is passive
            // (no connector, no other class).
            if counts.ic == 1
                && counts.connector == 0
                && counts.other == 0
                && counts.passive == fanout.saturating_sub(1)
            {
                single_ic_pin.push(AuditNetRow {
                    net: net.name.clone(),
                    code: net.code,
                    fanout,
                    note: "1 IC pin + only passives".to_string(),
                });
            }

            if fanout <= 1 {
                stub.push(AuditNetRow {
                    net: net.name.clone(),
                    code: net.code,
                    fanout,
                    note: "single-pin net".to_string(),
                });
            } else if counts.ic == 0 && counts.connector == 0 {
                stub.push(AuditNetRow {
                    net: net.name.clone(),
                    code: net.code,
                    fanout,
                    note: "only passives, no IC or connector".to_string(),
                });
            }
        }

        let envelope = AuditEnvelope {
            unpowered_power_in: Self::bucket_audit(unpowered_power_in, limit),
            undriven_input: Self::bucket_audit(undriven_input, limit),
            single_ic_pin: Self::bucket_audit(single_ic_pin, limit),
            stub: Self::bucket_audit(stub, limit),
        };
        return Ok(serde_json::to_string_pretty(&envelope).context("error serializing audit")?);
    }

    /// Sort an `audit` bucket (fanout desc, name asc), record its true count,
    /// then cap the returned rows at `limit`.
    fn bucket_audit(mut rows: Vec<AuditNetRow>, limit: u32) -> AuditBucket {
        rows.sort_by(|a, b| b.fanout.cmp(&a.fanout).then_with(|| a.net.cmp(&b.net)));
        let count = rows.len();
        rows.truncate(limit as usize);
        let returned = rows.len();
        AuditBucket { count, returned, nets: rows }
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

    /// Refdes class = leading non-digit prefix, uppercased. e.g. "R40" -> "R",
    /// "TP3" -> "TP". Shared by walk's passthrough/endpoint classification.
    fn refdes_class(refdes: &str) -> String {
        refdes
            .chars()
            .take_while(|c| !c.is_ascii_digit())
            .collect::<String>()
            .to_uppercase()
    }

    /// Map a via chain of passthrough components to the compact {refdes,value,class}
    /// rows the walk envelope reports.
    fn via_parts(&self, via: &[CompId]) -> Vec<ViaPart> {
        via.iter()
            .map(|c| {
                let comp = self.component(c);
                ViaPart {
                    refdes: comp.refdes.clone(),
                    value: comp.value.clone(),
                    class: Self::refdes_class(&comp.refdes),
                }
            })
            .collect()
    }

    /// Connectivity traversal: from a pin or net, follow the bipartite net<->pin
    /// graph THROUGH 2-pin series passives (R/L/FB/C) to the real opaque endpoints
    /// (ICs, connectors, transistors, ...), stopping at power rails and huge nets
    /// which are reported but never enumerated. Topological, not electrical.
    ///
    /// `include_topology` is accepted for API stability but ignored here — output
    /// is a flat endpoint list, not a branch tree.
    ///
    /// The BFS core lives in `walk_bfs`; a future `path_between` reuses it.
    pub fn walk(
        &self,
        start: &str,
        max_depth: u32,
        max_endpoints: u32,
        stop_at_power: bool,
        _include_topology: bool,
    ) -> anyhow::Result<String> {
        // Resolve the start: "REFDES:PIN" is a pin (start_comp excluded from
        // endpoints), otherwise a net name.
        let (start_net_id, start_comp): (NetId, Option<CompId>) = if start.contains(':') {
            let pin_id = self
                .pin_map
                .get(start)
                .with_context(|| format!("no pin named '{}'", start))?;
            let pin = self.pin(pin_id);
            let net_id = pin
                .net
                .as_ref()
                .with_context(|| format!("pin '{}' has no net", start))?;
            (NetId(net_id.0), Some(CompId(pin.comp.0)))
        } else {
            let net_id = self
                .net_map
                .get(start)
                .with_context(|| format!("no net called '{}'", start))?;
            (NetId(net_id.0), None)
        };

        let start_net_name = self.net(&start_net_id).name.clone();

        let mut data = self.walk_bfs(
            &start_net_id,
            start_comp.as_ref(),
            max_depth,
            max_endpoints,
            stop_at_power,
        );

        // Order endpoints: distance asc, then natural refdes (R2 before R10),
        // then natural pin number within a component.
        data.endpoints.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then_with(|| {
                    Self::pin_sort_key(&a.component.refdes)
                        .cmp(&Self::pin_sort_key(&b.component.refdes))
                })
                .then_with(|| {
                    let an = a.pin.rsplit(':').next().unwrap_or("");
                    let bn = b.pin.rsplit(':').next().unwrap_or("");
                    Self::pin_sort_key(an).cmp(&Self::pin_sort_key(bn))
                })
        });

        let envelope = WalkEnvelope {
            start: start.to_string(),
            start_net: start_net_name,
            endpoints: data.endpoints,
            rails_reached: data.rails_reached,
            large_nets: data.large_nets,
            truncated: data.truncated,
        };
        return Ok(serde_json::to_string_pretty(&envelope).context("error serializing walk")?);
    }

    /// BFS traversal core shared by `walk` (and, later, `path_between`). Alternates
    /// net -> pins -> owning component -> (through a passthrough?) -> other net.
    /// Rails (score >= 0.5 when `stop_at_power`) and large nets (fanout > 40) are
    /// terminal. Cycles are cut by `visited_nets` / `visited_comps`.
    fn walk_bfs(
        &self,
        start_net: &NetId,
        start_comp: Option<&CompId>,
        max_depth: u32,
        max_endpoints: u32,
        stop_at_power: bool,
    ) -> WalkData {
        let start_net_idx = start_net.0;
        let start_comp_idx = start_comp.map(|c| c.0);

        let mut visited_nets: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut visited_comps: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut endpoint_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut endpoints: Vec<WalkEndpoint> = Vec::new();
        let mut rails_reached: Vec<RailReached> = Vec::new();
        let mut large_nets: Vec<LargeNet> = Vec::new();
        let mut truncated = false;

        let mut queue: std::collections::VecDeque<(usize, Vec<CompId>, u32)> =
            std::collections::VecDeque::new();
        queue.push_back((start_net_idx, Vec::new(), 0));

        while let Some((net_idx, via, depth)) = queue.pop_front() {
            if !visited_nets.insert(net_idx) {
                continue;
            }
            let net = self.net(&NetId(net_idx));

            // Terminal checks apply to every net except the start net, which is
            // always expanded once.
            if net_idx != start_net_idx {
                if stop_at_power {
                    let (score, _evidence) = self.rail_score(net);
                    if score >= 0.5 {
                        rails_reached.push(RailReached {
                            net: net.name.clone(),
                            score: (score * 100.0).round() / 100.0,
                            via: self.via_parts(&via),
                        });
                        continue;
                    }
                }
                if net.pins.len() > 40 {
                    large_nets.push(LargeNet {
                        net: net.name.clone(),
                        fanout: net.pins.len(),
                        via: self.via_parts(&via),
                    });
                    continue;
                }
            }

            for pin_id in &net.pins {
                let pin = self.pin(pin_id);
                let comp_id = &pin.comp;

                // At the start net, never walk back into the start component.
                if via.is_empty() && Some(comp_id.0) == start_comp_idx {
                    continue;
                }
                // Already-traversed passthrough — avoid bouncing back.
                if visited_comps.contains(&comp_id.0) {
                    continue;
                }

                let comp = self.component(comp_id);
                let class = Self::refdes_class(&comp.refdes);
                let is_passthrough =
                    comp.pins.len() == 2 && matches!(class.as_str(), "R" | "L" | "FB" | "C");

                if is_passthrough {
                    // The OTHER pin: this comp's pin whose net differs from the
                    // current one. Exactly one such Some(net) pin => traverse.
                    let others: Vec<usize> = comp
                        .pins
                        .iter()
                        .filter_map(|p| self.pin(p).net.as_ref().map(|n| n.0))
                        .filter(|n| *n != net_idx)
                        .collect();
                    if others.len() != 1 {
                        // NC other pin, or both pins on the same net -> dead end.
                        continue;
                    }
                    let other_net = others[0];
                    visited_comps.insert(comp_id.0);
                    if depth < max_depth {
                        let mut next_via = via.clone();
                        next_via.push(CompId(comp_id.0));
                        queue.push_back((other_net, next_via, depth + 1));
                    } else {
                        // Depth-limited branch.
                        truncated = true;
                    }
                } else {
                    // Endpoint. Never report the start component.
                    if Some(comp_id.0) == start_comp_idx {
                        continue;
                    }
                    let pin_key = self.pin_name(pin_id);
                    if !endpoint_seen.insert(pin_key.clone()) {
                        continue;
                    }
                    if endpoints.len() >= max_endpoints as usize {
                        truncated = true;
                        continue;
                    }
                    endpoints.push(WalkEndpoint {
                        pin: pin_key,
                        pin_name: pin.name.clone(),
                        pin_type: pin.pin_type.clone(),
                        component: EndpointComponent {
                            refdes: comp.refdes.clone(),
                            value: comp.value.clone(),
                            description: comp.description.clone(),
                            sheet: comp.sheet.clone(),
                        },
                        via: self.via_parts(&via),
                        distance: depth,
                    });
                }
            }
        }

        WalkData {
            endpoints,
            rails_reached,
            large_nets,
            truncated,
        }
    }

    /// Report whether two pins are connected through the signal/passthrough
    /// graph (same semantics as `walk`: through 2-pin series R/L/FB/C, never
    /// through ICs, never across power rails) and, if so, the series parts on
    /// the path. Reuses `walk_bfs` from `from`'s net/component, then looks for
    /// `to` among the reached endpoints (or, failing that, among the rails
    /// reached, in case the only route is a shared power/ground net).
    pub fn path_between(&self, from: &str, to: &str) -> anyhow::Result<String> {
        let from_pin_id = self
            .pin_map
            .get(from)
            .with_context(|| format!("no pin named '{}'", from))?;
        let to_pin_id = self
            .pin_map
            .get(to)
            .with_context(|| format!("no pin named '{}'", to))?;

        let from_pin = self.pin(from_pin_id);
        let to_pin = self.pin(to_pin_id);

        let from_net_id = match &from_pin.net {
            Some(n) => NetId(n.0),
            None => {
                return Self::path_between_envelope(
                    from, to, false, None, Vec::new(),
                    Some("from pin is unconnected".to_string()),
                );
            }
        };

        // Trivial: same pin, or already on the same net (no passthrough hop
        // needed at all).
        if from_pin_id.0 == to_pin_id.0 {
            return Self::path_between_envelope(from, to, true, Some(0), Vec::new(), None);
        }
        if let Some(to_net) = &to_pin.net {
            if to_net.0 == from_net_id.0 {
                return Self::path_between_envelope(from, to, true, Some(0), Vec::new(), None);
            }
        }

        let from_comp = CompId(from_pin.comp.0);
        let to_canonical = self.pin_name(to_pin_id);

        // max_endpoints is effectively unbounded here: we need to search the
        // whole reachable set for `to`, not stop at the first handful.
        let data = self.walk_bfs(&from_net_id, Some(&from_comp), 6, 100_000, true);

        if let Some(ep) = data.endpoints.iter().find(|e| e.pin == to_canonical) {
            return Self::path_between_envelope(
                from,
                to,
                true,
                Some(ep.distance),
                Self::clone_via(&ep.via),
                None,
            );
        }

        // Not directly reached, but maybe the only route is through a shared
        // rail (power/ground) — match by the `to` pin's net name.
        if let Some(to_net_id) = &to_pin.net {
            let to_net_name = &self.net(to_net_id).name;
            if let Some(rail) = data.rails_reached.iter().find(|r| &r.net == to_net_name) {
                return Self::path_between_envelope(
                    from,
                    to,
                    true,
                    Some(rail.via.len() as u32),
                    Self::clone_via(&rail.via),
                    Some(format!(
                        "only via rail {} (shared power/ground, not a signal path)",
                        rail.net
                    )),
                );
            }
        }

        Self::path_between_envelope(
            from,
            to,
            false,
            None,
            Vec::new(),
            Some("no passthrough path within depth 6 (rails are not crossed)".to_string()),
        )
    }

    fn clone_via(via: &[ViaPart]) -> Vec<ViaPart> {
        via.iter()
            .map(|v| ViaPart {
                refdes: v.refdes.clone(),
                value: v.value.clone(),
                class: v.class.clone(),
            })
            .collect()
    }

    fn path_between_envelope(
        from: &str,
        to: &str,
        connected: bool,
        distance: Option<u32>,
        via: Vec<ViaPart>,
        note: Option<String>,
    ) -> anyhow::Result<String> {
        let envelope = PathBetweenEnvelope {
            from: from.to_string(),
            to: to.to_string(),
            connected,
            distance,
            via,
            note,
        };
        Ok(serde_json::to_string_pretty(&envelope).context("error serializing path_between")?)
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
                let net_no = netlist_pin.net.with_context(|| format!("no net code for pin {}:{}! netlist pin: {:?}", comp.refdes, netlist_pin.number, netlist_pin))?;
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

#[derive(Debug, Serialize, Clone)]
pub struct CompId(usize);

/// One pin in a `get_component` detail response.
#[derive(Debug, Serialize)]
struct ComponentPinRow {
    pin: String,
    name: Option<String>,
    #[serde(rename = "type")]
    pin_type: Option<String>,
    net: Option<String>,
}

/// The `get_component` output envelope: full identity/properties plus a
/// paginated, structured pin list.
#[derive(Debug, Serialize)]
struct ComponentDetail {
    refdes: String,
    value: String,
    description: Option<String>,
    keywords: Option<String>,
    footprint: Option<String>,
    sheet: Option<String>,
    properties: HashMap<String, Option<String>>,
    pin_count: usize,
    offset: u32,
    limit: u32,
    returned: usize,
    pins: Vec<ComponentPinRow>,
}

/// One member pin in a `get_net` detail response: the pin itself plus its
/// owning component's identity and the pin's own name/type.
#[derive(Debug, Serialize)]
struct NetMemberRow {
    pin: String,
    refdes: String,
    value: String,
    pin_name: Option<String>,
    #[serde(rename = "type")]
    pin_type: Option<String>,
}

/// The `get_net` output envelope: identity/fanout, rail-score with evidence,
/// pin-type histogram, connected subsystems, a factual role classification
/// (see `Design::net_role`), and paginated members.
#[derive(Debug, Serialize)]
struct NetDetail {
    net: String,
    code: usize,
    fanout: usize,
    rail_score: f32,
    rail_evidence: Vec<String>,
    pin_types: HashMap<String, i32>,
    subsystems: Vec<String>,
    role: NetRole,
    hierarchy: NetHierarchy,
    offset: u32,
    limit: u32,
    returned: usize,
    members: Vec<NetMemberRow>,
}

/// Factual classification of a net's role in the graph, derived purely from
/// pin-type presence and owning-component classes — no judgment about
/// whether the pattern is correct or intended. See `Design::net_role`.
#[derive(Debug, Serialize, Clone)]
pub struct NetRole {
    /// Any `power_out` pin present on the net.
    pub has_source: bool,
    /// Any driver-typed pin present (`output`, `bidirectional`, `tri_state`,
    /// `open_collector`, `open_emitter`, `power_out`).
    pub has_driver: bool,
    /// Any `power_in` pin present on the net.
    pub has_power_in: bool,
    /// Any `input` pin present on the net.
    pub has_input: bool,
    /// Count of pins whose owning component's refdes class is "U" (IC).
    pub ic_pin_count: usize,
    /// True if every pin's owning component is a passive class (R/L/C/FB) —
    /// i.e. no IC, connector, or other class touches this net.
    pub passive_only: bool,
}

/// Structural decomposition of a net name by its '/'-separated hierarchy.
/// This is a naming-structure HINT derived purely from the name string — it
/// is NOT an authoritative statement of electrical or schematic scope.
/// KiCad's netlist export does not cleanly distinguish global vs. local
/// nets, so this deliberately makes no claim about intended connectivity or
/// cross-sheet relationships; it only reports what the name's slash
/// structure looks like. See `Design::net_hierarchy`.
#[derive(Debug, Serialize, Clone)]
pub struct NetHierarchy {
    /// True if the net name starts with '/' (KiCad's hierarchical-path prefix).
    pub rooted: bool,
    /// The last '/'-separated segment of the name (the whole name if flat).
    pub local_name: String,
    /// All-but-last '/'-separated segments, rejoined with a leading '/', or
    /// `None` when there is only one segment (no path prefix).
    pub sheet_path: Option<String>,
    /// Count of path segments before `local_name` (0 for flat names like
    /// `GND` and for single-segment rooted names like `/FOO#`).
    pub depth: usize,
    /// "flat" if the name has no leading '/' (typically a power/global
    /// label such as GND or +3.3V), else "hierarchical". A naming-structure
    /// hint only — not a verified or guaranteed scope.
    pub scope_hint: &'static str,
}

/// Internal per-net tally of owning-component classes, computed by
/// `net_class_counts` and shared by `net_role` and `audit`.
#[derive(Debug, Default)]
struct NetClassCounts {
    ic: usize,
    connector: usize,
    passive: usize,
    other: usize,
}

/// One net in an `audit` bucket: identity/fanout plus a neutral, factual
/// note describing why it landed in that bucket (never a verdict).
#[derive(Debug, Serialize)]
struct AuditNetRow {
    net: String,
    code: usize,
    fanout: usize,
    note: String,
}

/// One `audit` category: the true count across the whole design, how many
/// rows were actually returned (capped by `limit`), and those rows.
#[derive(Debug, Serialize)]
struct AuditBucket {
    count: usize,
    returned: usize,
    nets: Vec<AuditNetRow>,
}

/// The `audit` output envelope: four non-exclusive FACTUAL categories over
/// the whole net graph. See `Design::audit`.
#[derive(Debug, Serialize)]
struct AuditEnvelope {
    unpowered_power_in: AuditBucket,
    undriven_input: AuditBucket,
    single_ic_pin: AuditBucket,
    stub: AuditBucket,
}

/// The owning component of a `get_pin` detail response (compact identity only).
#[derive(Debug, Serialize)]
struct PinComponentInfo {
    refdes: String,
    value: String,
    description: Option<String>,
    sheet: Option<String>,
}

/// The net of a `get_pin` detail response, or absent if the pin is unconnected.
#[derive(Debug, Serialize)]
struct PinNetInfo {
    name: String,
    code: usize,
    fanout: usize,
    rail_score: f32,
}

/// The `get_pin` output envelope.
#[derive(Debug, Serialize)]
struct PinDetail {
    pin: String,
    name: Option<String>,
    #[serde(rename = "type")]
    pin_type: Option<String>,
    component: PinComponentInfo,
    net: Option<PinNetInfo>,
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
/// `sheet_path`/`depth` are the compact half of `NetHierarchy` (see
/// `Design::net_hierarchy`) — a naming-structure hint, not a scope guarantee.
#[derive(Debug, Serialize)]
struct NetRow {
    name: String,
    code: usize,
    fanout: usize,
    pin_types: HashMap<String, i32>,
    sheet_path: Option<String>,
    depth: usize,
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

/// Internal result of the `walk_bfs` traversal core, before the endpoints are
/// sorted and wrapped in the public envelope.
struct WalkData {
    endpoints: Vec<WalkEndpoint>,
    rails_reached: Vec<RailReached>,
    large_nets: Vec<LargeNet>,
    truncated: bool,
}

/// One series part traversed on the way to an endpoint or terminal net.
#[derive(Debug, Serialize)]
struct ViaPart {
    refdes: String,
    value: String,
    class: String,
}

/// The owning component of a reached endpoint pin (compact identity only).
#[derive(Debug, Serialize)]
struct EndpointComponent {
    refdes: String,
    value: String,
    description: Option<String>,
    sheet: Option<String>,
}

/// One opaque endpoint reached by `walk`: the specific pin, its function, the
/// owning component, the series parts traversed (`via`), and hop distance.
#[derive(Debug, Serialize)]
struct WalkEndpoint {
    pin: String,
    pin_name: Option<String>,
    pin_type: Option<String>,
    component: EndpointComponent,
    via: Vec<ViaPart>,
    distance: u32,
}

/// A power/ground rail the walk stopped at (reported, never enumerated).
#[derive(Debug, Serialize)]
struct RailReached {
    net: String,
    score: f32,
    via: Vec<ViaPart>,
}

/// A high-fanout net (> 40 pins) the walk stopped at — catches supply rails that
/// score just under the rail threshold so they can't explode.
#[derive(Debug, Serialize)]
struct LargeNet {
    net: String,
    fanout: usize,
    via: Vec<ViaPart>,
}

/// The `walk` output envelope.
#[derive(Debug, Serialize)]
struct WalkEnvelope {
    start: String,
    start_net: String,
    endpoints: Vec<WalkEndpoint>,
    rails_reached: Vec<RailReached>,
    large_nets: Vec<LargeNet>,
    truncated: bool,
}

/// The `path_between` output envelope. `via` is empty for a direct same-net
/// (or same-pin) connection; `distance` is the passthrough hop count, null
/// when not connected.
#[derive(Debug, Serialize)]
struct PathBetweenEnvelope {
    from: String,
    to: String,
    connected: bool,
    distance: Option<u32>,
    via: Vec<ViaPart>,
    note: Option<String>,
}

/// Drop candidates below this so pure noise doesn't surface.
const SCORE_FLOOR: f32 = 0.15;

// Field weights for the fallback term-match tier in `score_component` (see
// `term_match_score`). Named consts so the relative trust placed in each
// field is visible and easy to retune. `sheet` is deliberately the lowest —
// a sheet is a schematic LOCATION, not a part identity, so a query that only
// matches a page name (e.g. "adc" hitting `/ADC1/`) must not compete with a
// query that actually matches the part's own value/keywords/description.
const TERM_WEIGHT_VALUE: f32 = 0.60;
const TERM_WEIGHT_KEYWORDS: f32 = 0.58;
const TERM_WEIGHT_DESCRIPTION: f32 = 0.48;
const TERM_WEIGHT_FOOTPRINT: f32 = 0.32;
const TERM_WEIGHT_SHEET: f32 = 0.25;

// Hand-tuned priors for `Design::rail_score`. Kept as named consts so they
// are easy to retune without hunting through the scoring logic.
const RAIL_WEIGHT_POWER_FRAC: f32 = 0.45;
const RAIL_WEIGHT_NAME_MATCH: f32 = 0.30;
const RAIL_WEIGHT_CAP_FRAC: f32 = 0.25;
const RAIL_FANOUT_BOOST: f32 = 0.15;

// Pin-type and refdes-class sets shared by `Design::net_role` and
// `Design::audit`. A "driver" is any pin type capable of actively asserting
// a level onto the net; `power_out` counts as both a driver and the sole
// "source" type.
const DRIVER_PIN_TYPES: &[&str] = &[
    "output", "bidirectional", "tri_state", "open_collector", "open_emitter", "power_out",
];
const PASSIVE_CLASSES: &[&str] = &["R", "L", "C", "FB"];
const CONNECTOR_CLASSES: &[&str] = &["J", "P"];
const IC_CLASS: &str = "U";

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

    // Tiers 4 & 5: field-weighted, token-aware term matching (see
    // `term_match_score`) — replaces the old flat "terms against the
    // flattened bundle" tier so a query that only hits a component's sheet
    // name doesn't score the same as one that hits its value or keywords.
    term_match_score(terms, comp)
}

/// Lowercase `s` and split on runs of non-alphanumeric characters into tokens.
/// Used by `term_match_score` so a term matches whole tokens (or a token
/// prefix), never a mid-word substring — "res" must not match "pressure".
fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// A query term "matches" a field if any of the field's tokens equals the
/// term or starts with it (prefix match: "adc" matches token "adc1", "spi"
/// matches "spi6").
fn term_matches_field(term: &str, tokens: &[String]) -> bool {
    tokens.iter().any(|tok| tok == term || tok.starts_with(term))
}

/// Field-weighted, token-aware fallback for `score_component`, reached only
/// when the exact/base-match/substring tiers (1-3) didn't fire. Scores each
/// named field independently (value, keywords, description, footprint,
/// sheet — see the `TERM_WEIGHT_*` consts) rather than one flattened bundle,
/// so the `match_reason` can name exactly which field earned the score.
///
/// If every query term matches a single field's tokens, that field "fires"
/// at its full weight; the max weight among firing fields wins. Otherwise
/// the best partial (highest `weight * matched/total` among fields with at
/// least one matching term) sets a capped score, kept below the weakest
/// full-field fire (`sheet` at `TERM_WEIGHT_SHEET`) so partial matches never
/// masquerade as a real field hit. `None` if nothing matches at all.
fn term_match_score(terms: &[&str], comp: &Component) -> Option<(f32, String)> {
    if terms.is_empty() {
        return None;
    }
    let n = terms.len();

    let keywords = comp.properties.get("ki_keywords").and_then(|v| v.as_deref());
    let fields: [(&str, f32, Option<&str>); 5] = [
        ("value", TERM_WEIGHT_VALUE, Some(comp.value.as_str())),
        ("keywords", TERM_WEIGHT_KEYWORDS, keywords),
        ("description", TERM_WEIGHT_DESCRIPTION, comp.description.as_deref()),
        ("footprint", TERM_WEIGHT_FOOTPRINT, comp.footprint.as_deref()),
        ("sheet", TERM_WEIGHT_SHEET, comp.sheet.as_deref()),
    ];

    let mut best_full: Option<(f32, &str)> = None;
    let mut best_partial: Option<(f32, usize, &str)> = None; // (weight*ratio, matched, field)

    for (label, weight, text) in fields {
        let Some(text) = text else { continue };
        let tokens = tokenize(text);
        if tokens.is_empty() {
            continue;
        }
        let matched = terms.iter().filter(|t| term_matches_field(t, &tokens)).count();
        if matched == 0 {
            continue;
        }
        if matched == n {
            if best_full.is_none_or(|(w, _)| weight > w) {
                best_full = Some((weight, label));
            }
        } else {
            let ratio_weighted = weight * (matched as f32 / n as f32);
            if best_partial.is_none_or(|(rw, _, _)| ratio_weighted > rw) {
                best_partial = Some((ratio_weighted, matched, label));
            }
        }
    }

    if let Some((weight, label)) = best_full {
        return Some((weight, format!("all terms in {label}")));
    }
    if let Some((ratio_weighted, matched, label)) = best_partial {
        let score = 0.15 + 0.30 * ratio_weighted;
        return Some((score, format!("matched {matched}/{n} terms in {label}")));
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