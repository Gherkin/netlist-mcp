use std::collections::HashMap;
use std::collections::HashSet;
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
    net_map: HashMap<String, NetId>,
    // Every sheet path this design actually has, normalized by
    // `normalize_sheet_path` and closed over ancestors. Read only by
    // `net_hierarchy`, to tell a hierarchy separator in a net name from a
    // slash that is part of a label (issue #16).
    sheet_paths: HashSet<String>
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
            value_norm: comp.value_norm.clone(),
            description: comp.description.clone(),
            keywords: comp.properties.get("ki_keywords").cloned().flatten(),
            footprint: comp.footprint.clone(),
            sheet: comp.sheet.clone(),
            dnp: comp.dnp,
            exclude_from_bom: comp.exclude_from_bom,
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
                    dnp: comp.dnp,
                    pin_name: pin.name.clone(),
                    pin_type: pin.pin_type.clone(),
                }
            })
            .collect();

        let role = self.net_role(net_ref);
        let hierarchy = self.net_hierarchy(&net_ref.name);

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
                dnp: comp.dnp,
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
                    dnp: nc.dnp,
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
            dnp: comp.dnp,
            net_groups,
        };
        return Ok(serde_json::to_string_pretty(&envelope)
            .context("error serializing neighbors")?);
    }

    /// Parse a `subsystem` argument against the sheets this design actually
    /// has — see `SubsystemFilter`. `None` means "no filter".
    ///
    /// The second half of the pair is set only when the argument was not taken
    /// at face value, i.e. a rooted path named no sheet and widened. Callers
    /// put it in the envelope's `subsystem_note`: the rows themselves cannot
    /// carry the news (a net's `sheet_path` comes from its *name*, and a
    /// component page shows only the sheets that fit under `limit`), so
    /// without it a widened answer is indistinguishable from an exact one.
    fn subsystem_filter(&self, arg: Option<&str>) -> (Option<SubsystemFilter>, Option<String>) {
        let Some(parsed) = SubsystemFilter::parse(arg) else {
            return (None, None);
        };
        // Declared sheets as well as the components': a sheet that holds no
        // parts is still a sheet, and asking for it should come back empty
        // rather than widened — the widening note would claim the design has
        // no such sheet, which by then a net's `sheet_path` has already named.
        let sheets = self.components.iter().map(|c| c.sheet.as_deref())
            .chain(self.sheet_paths.iter().map(|s| Some(s.as_str())));
        let filter = parsed.widen_if_unmatched(sheets);
        // PathPrefix is unreachable from parse, so its presence *is* the
        // record that widening happened. The note describes the widening
        // rather than claiming a match: the prefix may well match nothing
        // (a design whose only USB sheet is /PeriphUSB/ answers "/USB" with
        // an empty page), and that is the case where the caller most needs
        // the unanchored reading pointed out to them.
        let note = match &filter {
            SubsystemFilter::PathPrefix(prefix) => Some(format!(
                "no sheet in this design answers to '{}'; widened to sheets whose \
                 path starts with '{prefix}' — for an unanchored match, drop the \
                 leading slash",
                arg.unwrap_or_default().trim(),
            )),
            _ => None,
        };
        (Some(filter), note)
    }

    pub fn filter_components(
        &self,
        query: Option<&str>,
        refdes_class: Option<&str>,
        subsystem: Option<&str>,
        dnp: Option<bool>,
        limit: u32,
        offset: u32,
    ) -> anyhow::Result<String> {
        // Split the query into lowercased terms; treat empty/whitespace-only as no filter.
        let query_terms: Option<Vec<String>> = query
            .map(|q| q.split_whitespace().map(|t| t.to_lowercase()).collect::<Vec<_>>())
            .filter(|terms: &Vec<String>| !terms.is_empty());
        let refdes_class_lc = refdes_class.map(|c| c.to_lowercase());
        let (subsystem_filter, subsystem_note) = self.subsystem_filter(subsystem);

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

                // subsystem: rooted paths match as paths, bare names as
                // substrings — see `SubsystemFilter`.
                if let Some(filter) = &subsystem_filter {
                    if !filter.matches(comp.sheet.as_deref()) {
                        return false;
                    }
                }

                // dnp: tri-state — None means "don't care", not "populated only".
                if let Some(want_dnp) = dnp {
                    if comp.dnp != want_dnp {
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
                value_norm: comp.value_norm.clone(),
                description: comp.description.clone(),
                footprint: comp.footprint.clone(),
                sheet: comp.sheet.clone(),
                keywords: comp.properties.get("ki_keywords").cloned().flatten(),
                dnp: comp.dnp,
                pin_count: comp.pins.len(),
            })
            .collect();

        let envelope = FilterEnvelope {
            total,
            offset,
            limit,
            returned: rows.len(),
            subsystem_note,
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
                    value_norm: comp.value_norm.clone(),
                    description: comp.description.clone(),
                    footprint: comp.footprint.clone(),
                    sheet: comp.sheet.clone(),
                    keywords: comp.properties.get("ki_keywords").cloned().flatten(),
                    dnp: comp.dnp,
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
    ///
    /// One exception to "no scoring": under a `subsystem` filter every row is
    /// rail-scored and the rails sort last (issue #4). A net belongs to a
    /// subsystem only through the parts it touches, so GND and the supply rails
    /// match *every* sheet, and fanout-descending order then floats them above
    /// the sheet-local signals the query was after. They are demoted rather
    /// than dropped: whether GND reaches this sheet is a fair question, and a
    /// silently truncated answer is worse than a reordered one.
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
        let (subsystem_filter, subsystem_note) = self.subsystem_filter(subsystem);

        let matches: Vec<&Net> = self.nets
            .iter()
            .filter(|net| {
                // name: case-insensitive substring against the net name.
                if let Some(n) = &name_lc {
                    if !net.name.to_lowercase().contains(n.as_str()) {
                        return false;
                    }
                }

                // subsystem: any connected component sits on a matching sheet
                // (same matcher as filter_components).
                if let Some(filter) = &subsystem_filter {
                    let hit = self.net_component_sheets(net)
                        .any(|sheet| filter.matches(Some(sheet)));
                    if !hit {
                        return false;
                    }
                }

                true
            })
            .collect();

        // Rail-scoring is a subsystem-filter concern only: an unfiltered page
        // (or a name search for "gnd") is asking about the design as a whole,
        // where a high-fanout rail on top is the right answer.
        // Rounded here, once, and then both ranked and reported at that
        // precision: a row that reads `0.5` has to be one of the rails the note
        // is about. Comparing the raw score while reporting a rounded one lets a
        // net just under the threshold print as exactly `0.5` and still sort
        // first, which is unanswerable from the row alone.
        let scored = subsystem_filter.is_some();
        let mut matches: Vec<(&Net, f32)> = matches
            .into_iter()
            .map(|net| {
                let score = if scored { self.rail_score(net).0 } else { 0.0 };
                (net, (score * 100.0).round() / 100.0)
            })
            .collect();

        // Rails last (no-op without a subsystem filter, where every score is 0),
        // then within each group: default (sort_by_fanout) fanout descending,
        // tie-break net name ascending; otherwise alphabetical by net name.
        matches.sort_by(|(a, a_score), (b, b_score)| {
            let a_rail = *a_score >= RAIL_THRESHOLD;
            let b_rail = *b_score >= RAIL_THRESHOLD;
            a_rail.cmp(&b_rail).then_with(|| {
                if sort_by_fanout {
                    b.pins.len().cmp(&a.pins.len()).then_with(|| a.name.cmp(&b.name))
                } else {
                    a.name.cmp(&b.name)
                }
            })
        });

        // Say so on the envelope: the demotion is invisible from the rows alone.
        // Name the demoted nets and where the block starts, because a `limit`
        // shorter than the match count pushes them onto a later page — pointing
        // at "each row's rail_score" would then point at rows that are not here.
        let total = matches.len();
        let demoted = matches.iter().filter(|(_, s)| *s >= RAIL_THRESHOLD).count();
        let rail_start = total - demoted;
        let rail_note = (demoted > 0).then(|| {
            let shown = RAIL_NOTE_NAMES.min(demoted);
            let mut named: String = matches[rail_start..rail_start + shown]
                .iter()
                .map(|(net, _)| net.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if demoted > shown {
                named.push_str(&format!(", +{} more", demoted - shown));
            }
            format!(
                "{demoted} power/ground rail(s) sorted last: {named}. A rail \
                 reaches parts on nearly every sheet, so it matches this \
                 subsystem without belonging to it. They are the final \
                 {demoted} of {total} matches, from offset {rail_start} — \
                 request that offset if this page stops short of them.",
            )
        });
        let rows: Vec<NetRow> = matches
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|(net, rail)| {
                let hierarchy = self.net_hierarchy(&net.name);
                NetRow {
                    name: net.name.clone(),
                    code: net.code,
                    fanout: net.pins.len(),
                    pin_types: net.pin_types.clone(),
                    sheet_path: hierarchy.sheet_path,
                    depth: hierarchy.depth,
                    rail_score: scored.then_some(rail),
                }
            })
            .collect();

        let envelope = NetEnvelope {
            total,
            offset,
            limit,
            returned: rows.len(),
            subsystem_note,
            rail_note,
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
                    let name = subsystem_display_name(&path);
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
    /// connector ("J"/"P"/"CN"/"RJ"/"USB"), passive ("R"/"L"/"C"/"FB"), and
    /// everything else, plus an `endpoint` tally (see `is_endpoint_class`)
    /// that cuts across the first three. Shared by `net_role` and `audit` so
    /// both agree on what "IC pin", "connector pin", "endpoint pin", and
    /// "passive-only" mean.
    fn net_class_counts(&self, net: &Net) -> NetClassCounts {
        let mut counts = NetClassCounts::default();
        for pid in &net.pins {
            let comp = self.component(&self.pin(pid).comp);
            let class = Self::refdes_class(&comp.refdes);
            let pin_count = comp.pins.len();
            if is_endpoint_class(&class, pin_count) {
                counts.endpoint += 1;
            }
            if class == IC_CLASS {
                counts.ic += 1;
            } else if CONNECTOR_CLASSES.contains(&class.as_str()) {
                counts.connector += 1;
            } else if is_passive_passthrough(&class, pin_count) {
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
            endpoint_pin_count: counts.endpoint,
            passive_only,
        }
    }

    /// Decomposition of a net name into the sheet path KiCad prefixed it with
    /// and the label a human typed. Does not infer intended connectivity,
    /// scope, or cross-sheet bridging. Reused by `get_net` (embedded as
    /// `hierarchy`) and `filter_nets` (compact `sheet_path`/`depth` fields on
    /// each row).
    ///
    /// The split is against `self.sheet_paths`, not against '/' — see
    /// `net_hierarchy_in` for why the naive split is wrong.
    pub fn net_hierarchy(&self, name: &str) -> NetHierarchy {
        Self::net_hierarchy_in(name, &self.sheet_paths)
    }

    /// The sheet-set half of `net_hierarchy`, split out to be testable
    /// without a whole design behind it.
    ///
    /// A net name is `<sheet path><label>`, and both halves may contain '/':
    /// a label named after a dual-function pin ("LED1/REGOFF", after the
    /// LAN8720A's LED1/nREGOFF) is normal. Splitting on the *last* slash
    /// therefore invents a sheet — "/Ethernet/LED1/REGOFF" reads as a net on
    /// "/Ethernet/LED1", a sheet no design has (issue #16), and the depth
    /// that comes with it is wrong too.
    ///
    /// So the prefix is matched against the sheets the design actually has,
    /// longest first (a design with both "/Ethernet/" and "/Ethernet/PHY/"
    /// splits "/Ethernet/PHY/RST" at the deeper one). A name whose prefix
    /// names no sheet is reported flat — whole name as `local_name`, no
    /// `sheet_path`, depth 0. That is the honest reading for a global label
    /// and for a local label containing a slash alike, and it never points at
    /// a sheet that does not exist.
    fn net_hierarchy_in(name: &str, sheets: &HashSet<String>) -> NetHierarchy {
        let rooted = name.starts_with('/');
        let segments: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();

        // Longest segment prefix that names a real sheet; 0 when none does,
        // which includes every flat name (there is no prefix to test).
        let sheet_segments = (1..segments.len())
            .rev()
            .find(|k| sheets.contains(&normalize_sheet_path(&segments[..*k].join("/"))))
            .unwrap_or(0);

        let local_name = if sheet_segments == 0 {
            // Not `segments.join("/")`: that would silently normalize away a
            // leading or doubled slash the caller may want to see echoed.
            name.trim_start_matches('/').to_string()
        } else {
            segments[sheet_segments..].join("/")
        };
        let sheet_path = (sheet_segments > 0)
            .then(|| format!("/{}", segments[..sheet_segments].join("/")));
        let scope_hint = if rooted { "hierarchical" } else { "flat" };

        NetHierarchy { rooted, local_name, sheet_path, depth: sheet_segments, scope_hint }
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
    /// - `stub`: a single-pin (or unconnected) net, or a multi-pin net that
    ///   reaches no endpoint part at all — only two-terminal passives and
    ///   probe/mechanical parts (see `is_endpoint_class`). The note names
    ///   whichever of the two the net actually carries.
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
            } else if counts.endpoint == 0 {
                // Every pin is a two-terminal passive or a probe/mechanical
                // part; say which are actually present rather than naming both
                // for a net that carries only one (a chassis net of mounting
                // holes has no passives on it at all).
                let note = match (counts.passive, fanout - counts.passive) {
                    (0, _) => "only probe/mechanical parts, no endpoint part",
                    (_, 0) => "only passives, no endpoint part",
                    _ => "only passives and probe/mechanical parts, no endpoint part",
                };
                stub.push(AuditNetRow {
                    net: net.name.clone(),
                    code: net.code,
                    fanout,
                    note: note.to_string(),
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
            dnp_components: self.components.iter().filter(|c| c.dnp).count(),
            bom_excluded_components: self.components.iter().filter(|c| c.exclude_from_bom).count(),
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

        // rails: every net at or over `RAIL_THRESHOLD`, sorted score desc then
        // fanout desc, capped 25.
        let mut rails: Vec<RailRow> = self.nets
            .iter()
            .filter_map(|net| {
                let (score, evidence) = self.rail_score(net);
                if score >= RAIL_THRESHOLD {
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

        // connectors: any CONNECTOR_CLASSES refdes, natural order, capped 50.
        let mut connectors: Vec<&Component> = self.components
            .iter()
            .filter(|comp| {
                CONNECTOR_CLASSES.contains(&Self::refdes_class(&comp.refdes).as_str())
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
                dnp: comp.dnp,
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
                    Some(path) => subsystem_display_name(path),
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

    /// Map a refdes class to `walk`'s endpoint `kind` tag: a broad function
    /// grouping for opaque endpoints. Rails are reported separately
    /// (`rails_reached`), so there is no "rail" kind here.
    fn endpoint_kind(class: &str) -> String {
        match class {
            "U" => "ic",
            "J" | "P" | "CN" | "RJ" | "USB" => "connector",
            "D" => "diode",
            "Q" => "transistor",
            "T" => "transformer",
            "X" | "Y" => "crystal",
            "SW" => "switch",
            "TP" => "test_point",
            // Only reachable for a passive that is NOT a 2-pin passthrough:
            // a common-mode choke, an EMI filter, a resistor network.
            "R" | "L" | "C" | "FB" => "passive",
            _ => "other",
        }
        .to_string()
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
                    dnp: comp.dnp,
                }
            })
            .collect()
    }

    /// Connectivity traversal: from a pin or net, follow the bipartite net<->pin
    /// graph THROUGH 2-pin series passives (R/L/FB/C) to the real opaque endpoints
    /// (ICs, connectors, transistors, ...), stopping at power rails and huge nets
    /// which are reported but never enumerated. Topological, not electrical.
    ///
    /// `include_topology` adds a `topology` object: the nets the walk touched
    /// as nodes and the parts as edges, each part with the pin number and net
    /// of every side (see `build_topology`). Without it the output is the flat
    /// endpoint list only, unchanged. `max_topology_parts` caps the parts in
    /// that graph, nearest first (minimum 1); it does not change `endpoints`.
    /// `max_endpoints` caps the `endpoints` and `dead_ends` lists and the
    /// repeat `rails_reached` entries (issue #31), not the graph: endpoint
    /// parts it drops are still graph candidates (issue #35).
    ///
    /// The BFS core lives in `walk_bfs`, shared with `path_between`.
    pub fn walk(
        &self,
        start: &str,
        max_depth: u32,
        max_endpoints: u32,
        stop_at_power: bool,
        include_topology: bool,
        max_topology_parts: u32,
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
            include_topology,
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

        let topology = include_topology.then(|| {
            let order = self.walk_part_order(&data.topo, &data.endpoints, start_comp.as_ref());
            self.build_topology(
                &data.topo.nets,
                order,
                &[start_net_id.0],
                // At least 1, so the start part is always kept.
                max_topology_parts.max(1) as usize,
            )
        });

        let envelope = WalkEnvelope {
            start: start.to_string(),
            start_net: start_net_name,
            endpoints: data.endpoints,
            rails_reached: data.rails_reached,
            large_nets: data.large_nets,
            dead_ends: data.dead_ends,
            truncated: data.truncated,
            topology,
        };
        return Ok(serde_json::to_string_pretty(&envelope).context("error serializing walk")?);
    }

    /// BFS traversal core shared by `walk` and `path_between`. Alternates
    /// net -> pins -> owning component -> (through a passthrough?) -> other net.
    /// Rails (`RAIL_THRESHOLD` when `stop_at_power`) and large nets (fanout > 40) are
    /// terminal. Cycles are cut by `visited_nets` / `visited_comps`.
    ///
    /// Also surfaces `dead_ends`: branches that fizzle out through a passthrough
    /// (never the start net itself) instead of reaching a real endpoint — a
    /// passthrough whose far pin is NC/single-pin ("dangling"), or a reached net
    /// with no endpoint part on it at all ("passive_only" — see
    /// `is_endpoint_class`). These are additive/diagnostic and never change
    /// which endpoints/rails/large_nets are reported.
    fn walk_bfs(
        &self,
        start_net: &NetId,
        start_comp: Option<&CompId>,
        max_depth: u32,
        max_endpoints: u32,
        stop_at_power: bool,
        record_topology: bool,
    ) -> WalkData {
        let start_net_idx = start_net.0;
        let start_comp_idx = start_comp.map(|c| c.0);

        let mut visited_nets: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut visited_comps: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut endpoint_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut dead_end_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut endpoints: Vec<WalkEndpoint> = Vec::new();
        let mut rails_reached: Vec<RailReached> = Vec::new();
        let mut large_nets: Vec<LargeNet> = Vec::new();
        let mut dead_ends: Vec<DeadEnd> = Vec::new();
        let mut truncated = false;
        let mut topo = TopoTrace::new(record_topology);

        let mut queue: std::collections::VecDeque<(usize, Vec<CompId>, u32)> =
            std::collections::VecDeque::new();
        queue.push_back((start_net_idx, Vec::new(), 0));

        // Rails already stopped at, with their rounded score. A rail is never
        // expanded, so a later arrival over a different series part would be
        // cut by `visited_nets` and lost (issue #31). Each such arrival gets
        // its own `rails_reached` entry instead.
        let mut rail_scores: HashMap<usize, f32> = HashMap::new();

        while let Some((net_idx, via, depth)) = queue.pop_front() {
            if !visited_nets.insert(net_idx) {
                if let Some(&score) = rail_scores.get(&net_idx) {
                    // A repeat arrival. Each part is traversed once, so the
                    // via chain is new. A repeat is kept only while the whole
                    // `rails_reached` list (first arrivals included) is under
                    // `max_endpoints`. First arrivals are never cut, so the
                    // list can still end up longer than the cap.
                    if rails_reached.len() >= max_endpoints as usize {
                        truncated = true;
                    } else {
                        rails_reached.push(RailReached {
                            net: self.net(&NetId(net_idx)).name.clone(),
                            score,
                            via: self.via_parts(&via),
                        });
                    }
                }
                continue;
            }
            let net = self.net(&NetId(net_idx));

            // Terminal checks apply to every net except the start net, which is
            // always expanded once.
            if net_idx != start_net_idx {
                if stop_at_power {
                    let (score, _evidence) = self.rail_score(net);
                    if score >= RAIL_THRESHOLD {
                        topo.note_net(net_idx, NetState::Rail);
                        let score = (score * 100.0).round() / 100.0;
                        rail_scores.insert(net_idx, score);
                        rails_reached.push(RailReached {
                            net: net.name.clone(),
                            score,
                            via: self.via_parts(&via),
                        });
                        continue;
                    }
                }
                if net.pins.len() > 40 {
                    topo.note_net(net_idx, NetState::LargeNet);
                    large_nets.push(LargeNet {
                        net: net.name.clone(),
                        fanout: net.pins.len(),
                        via: self.via_parts(&via),
                    });
                    continue;
                }

                // passive_only dead end: this net was reached through at least
                // one passthrough (via is never empty here) and reaches no
                // endpoint part (`is_endpoint_class`) — only two-terminal
                // passives and probe/mechanical parts. Both go into `parts`:
                // a caller reading "no endpoint" needs to see the test point
                // that is nevertheless sitting on the net, because `endpoints`
                // reports it too.
                let mut has_active = false;
                let mut has_probe = false;
                let mut inert_seen: std::collections::HashSet<usize> =
                    std::collections::HashSet::new();
                let mut inert_parts: Vec<ViaPart> = Vec::new();
                for pin_id in &net.pins {
                    let pin = self.pin(pin_id);
                    let comp = self.component(&pin.comp);
                    let class = Self::refdes_class(&comp.refdes);
                    if is_endpoint_class(&class, comp.pins.len()) {
                        has_active = true;
                    } else {
                        has_probe |= PROBE_CLASSES.contains(&class.as_str());
                        if inert_seen.insert(pin.comp.0) {
                            inert_parts.push(ViaPart {
                                refdes: comp.refdes.clone(),
                                value: comp.value.clone(),
                                class,
                                dnp: comp.dnp,
                            });
                        }
                    }
                }
                if !has_active
                    && dead_end_seen.insert(net.name.clone())
                    && dead_ends.len() < max_endpoints as usize
                {
                    let reason = if has_probe {
                        "only passives and probe/mechanical parts, no active endpoint"
                    } else {
                        "only passives, no active endpoint"
                    };
                    dead_ends.push(DeadEnd {
                        net: Some(net.name.clone()),
                        fanout: net.pins.len(),
                        via: self.via_parts(&via),
                        parts: inert_parts,
                        reason: reason.to_string(),
                    });
                }
            }
            topo.note_net(net_idx, NetState::Expanded);

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
                let is_passthrough = is_passive_passthrough(&class, comp.pins.len());

                if is_passthrough {
                    // The OTHER pin on this 2-pin component (not the one we
                    // arrived on).
                    let other_pin_id = comp.pins.iter().find(|p| p.0 != pin_id.0);
                    let other_pin_has_no_net = other_pin_id
                        .map(|p| self.pin(p).net.is_none())
                        .unwrap_or(false);
                    let other_net_idx: Option<usize> = other_pin_id
                        .and_then(|p| self.pin(p).net.as_ref().map(|n| n.0))
                        .filter(|n| *n != net_idx);

                    visited_comps.insert(comp_id.0);
                    topo.note_series(comp_id.0, depth);

                    if other_pin_has_no_net {
                        // Far pin is NC -> dangling, goes nowhere.
                        let mut via_here = via.clone();
                        via_here.push(CompId(comp_id.0));
                        let key = format!("nc:{}", comp.refdes);
                        if dead_end_seen.insert(key) && dead_ends.len() < max_endpoints as usize {
                            dead_ends.push(DeadEnd {
                                net: None,
                                fanout: 0,
                                via: self.via_parts(&via_here),
                                parts: Vec::new(),
                                reason: "dangling (goes nowhere)".to_string(),
                            });
                        }
                        continue;
                    }
                    let Some(other_net_idx) = other_net_idx else {
                        // Both leads land on the same net (short/loop) —
                        // nothing further to traverse.
                        continue;
                    };
                    let other_net = self.net(&NetId(other_net_idx));
                    if other_net.pins.len() == 1 {
                        // Far pin's net has fanout 1 (only that pin) ->
                        // dangling, goes nowhere.
                        topo.note_net(other_net_idx, NetState::Dangling);
                        let mut via_here = via.clone();
                        via_here.push(CompId(comp_id.0));
                        if dead_end_seen.insert(other_net.name.clone())
                            && dead_ends.len() < max_endpoints as usize
                        {
                            dead_ends.push(DeadEnd {
                                net: Some(other_net.name.clone()),
                                fanout: 1,
                                via: self.via_parts(&via_here),
                                parts: Vec::new(),
                                reason: "dangling (goes nowhere)".to_string(),
                            });
                        }
                        continue;
                    }
                    if depth < max_depth {
                        let mut next_via = via.clone();
                        next_via.push(CompId(comp_id.0));
                        queue.push_back((other_net_idx, next_via, depth + 1));
                    } else {
                        // Depth-limited branch.
                        topo.note_net(other_net_idx, NetState::DepthLimit);
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
                        // Still a graph candidate (issue #35): only
                        // `max_topology_parts` limits the graph.
                        topo.note_dropped_endpoint(comp_id.0, depth);
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
                            dnp: comp.dnp,
                        },
                        kind: Self::endpoint_kind(&class),
                        via: self.via_parts(&via),
                        distance: depth,
                        comp_idx: comp_id.0,
                        net_idx,
                    });
                }
            }
        }

        WalkData {
            endpoints,
            rails_reached,
            large_nets,
            dead_ends,
            truncated,
            reached_net_count: visited_nets.len(),
            topo,
        }
    }

    /// The `walk` topology's parts in keep order (issue #34): the start part
    /// first, then series and endpoint parts by hop distance, series parts
    /// before endpoints at the same distance, so a cap spends itself on the
    /// branch structure before leaf ICs. Every net a kept series part reaches
    /// comes before any part one hop further, so a cut keeps a connected
    /// graph around the start. That includes endpoint parts `max_endpoints`
    /// kept out of `endpoints` (issue #35). The list can repeat a part;
    /// `build_topology` keeps its first place.
    fn walk_part_order(
        &self,
        topo: &TopoTrace,
        endpoints: &[WalkEndpoint],
        start_comp: Option<&CompId>,
    ) -> Vec<(usize, &'static str)> {
        let mut ranked: Vec<(u32, u8, usize, &'static str)> = Vec::new();
        for &(c, d) in &topo.series {
            ranked.push((d, 0, c, "series"));
        }
        // Endpoint parts `max_endpoints` dropped from the list are still in
        // the graph (issue #35). Listed and dropped sort together by distance
        // and natural refdes, so the graph does not depend on that cap.
        let mut ends: Vec<(usize, u32)> = endpoints.iter().map(|e| (e.comp_idx, e.distance)).collect();
        ends.extend(topo.dropped_endpoints.iter().copied());
        // The part index breaks `pin_sort_key` ties (R1 vs R01), so listed
        // parts do not sort ahead of dropped ones.
        ends.sort_by_cached_key(|&(c, d)| {
            (d, Self::pin_sort_key(&self.component(&CompId(c)).refdes), c)
        });
        for (c, d) in ends {
            ranked.push((d, 1, c, "endpoint"));
        }
        // Stable: series keep BFS order, endpoints keep the order above.
        ranked.sort_by_key(|&(d, r, _, _)| (d, r));
        start_comp
            .map(|c| (c.0, "start"))
            .into_iter()
            .chain(ranked.into_iter().map(|(_, _, c, role)| (c, role)))
            .collect()
    }

    /// Build a topology graph from nets the BFS recorded and parts in keep
    /// order (`walk_part_order`, `path_topology`). Nets are the nodes; parts
    /// are the edges. A "series" part lists all its pins; any other part
    /// lists only the pins on a kept net (in any state, rail and depth_limit
    /// included), so a 100-pin IC shows only its pins in the graph. A part
    /// listed twice keeps its first place and role.
    ///
    /// At most `max_parts` parts are kept (issue #34), from the front of
    /// `order`. Nets are then kept only when they are in `fixed_nets` or sit
    /// on a pin of a kept series part, so no node hangs loose. `nets` must
    /// hold every such net. `truncated`, `omitted_parts` and `omitted_nets`
    /// report the cut; each net's own `omitted_parts` counts the cut parts
    /// with a pin on it.
    fn build_topology(
        &self,
        nets: &[(usize, NetState)],
        order: Vec<(usize, &'static str)>,
        fixed_nets: &[usize],
        max_parts: usize,
    ) -> Topology {
        // Deduplicate: an endpoint part shows up once per reached pin, and
        // the start part can also be traversed as a series part from its far
        // side.
        let mut seen: HashSet<usize> = HashSet::new();
        let mut order: Vec<(usize, &'static str)> =
            order.into_iter().filter(|&(c, _)| seen.insert(c)).collect();
        let cut = order.split_off(max_parts.min(order.len()));
        let omitted_parts = cut.len();

        // Nets on a kept series part, plus the fixed nets. Other parts add no
        // nets: their pins are filtered to this set below.
        let mut kept_nets: HashSet<usize> = fixed_nets.iter().copied().collect();
        for &(c, role) in &order {
            if role == "series" {
                for p in &self.component(&CompId(c)).pins {
                    if let Some(n) = &self.pin(p).net {
                        kept_nets.insert(n.0);
                    }
                }
            }
        }
        // `kept_nets` is a subset of `nets`: `walk_bfs` notes every net it
        // pops and every far-side net of a series part it traverses, and
        // `path_topology` keeps every net of the parts it keeps.
        debug_assert!(kept_nets.iter().all(|n| nets.iter().any(|(t, _)| t == n)));
        let omitted_nets = nets.len().saturating_sub(kept_nets.len());

        // Per kept net: how many cut parts have a pin on it.
        let mut cut_on_net: HashMap<usize, usize> = HashMap::new();
        let mut on: HashSet<usize> = HashSet::new();
        for &(c, _) in &cut {
            on.clear();
            on.extend(self
                .component(&CompId(c))
                .pins
                .iter()
                .filter_map(|p| self.pin(p).net.as_ref().map(|n| n.0))
                .filter(|n| kept_nets.contains(n)));
            for &n in &on {
                *cut_on_net.entry(n).or_default() += 1;
            }
        }

        let nets = nets
            .iter()
            .filter(|(idx, _)| kept_nets.contains(idx))
            .map(|(idx, state)| {
                let net = self.net(&NetId(*idx));
                let (score, _) = self.rail_score(net);
                TopoNet {
                    name: net.name.clone(),
                    fanout: net.pins.len(),
                    rail: score >= RAIL_THRESHOLD,
                    state: state.as_str(),
                    omitted_parts: cut_on_net.get(idx).copied().unwrap_or(0),
                }
            })
            .collect();

        let mut parts: Vec<TopoPart> = Vec::new();
        for (comp_idx, role) in order {
            let all_pins = role == "series";
            let comp = self.component(&CompId(comp_idx));
            let mut pins: Vec<TopoPin> = comp
                .pins
                .iter()
                .map(|p| self.pin(p))
                .filter(|p| all_pins || p.net.as_ref().is_some_and(|n| kept_nets.contains(&n.0)))
                .map(|p| TopoPin {
                    pin: p.number.clone(),
                    name: p.name.clone(),
                    net: p.net.as_ref().map(|n| self.net(n).name.clone()),
                })
                .collect();
            pins.sort_by(|a, b| Self::pin_sort_key(&a.pin).cmp(&Self::pin_sort_key(&b.pin)));
            parts.push(TopoPart {
                refdes: comp.refdes.clone(),
                value: comp.value.clone(),
                class: Self::refdes_class(&comp.refdes),
                dnp: comp.dnp,
                role,
                pins,
            });
        }

        Topology {
            truncated: omitted_parts > 0,
            omitted_parts,
            omitted_nets,
            nets,
            parts,
        }
    }

    /// Report whether two pins are connected through the signal/passthrough
    /// graph (same semantics as `walk`: through 2-pin series R/L/FB/C, never
    /// through ICs, never across power rails) and, if so, the series parts on
    /// the path. Reuses `walk_bfs` from `from`'s net/component, then looks for
    /// `to` among the reached endpoints (or, failing that, among the rails
    /// reached, in case the only route is a shared power/ground net).
    ///
    /// `include_topology` adds a `topology` object when the pins are
    /// connected: the same shape as `walk`'s, pruned to the from→to subgraph
    /// (see `path_topology`), capped at `max_topology_parts` (minimum 2, so
    /// both end parts are kept). Without it the output is unchanged.
    pub fn path_between(
        &self,
        from: &str,
        to: &str,
        include_topology: bool,
        max_topology_parts: u32,
    ) -> anyhow::Result<String> {
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
                    None,
                    None,
                );
            }
        };

        // Trivial: same pin, or already on the same net (no passthrough hop
        // needed at all).
        let same_net = from_pin_id.0 == to_pin_id.0
            || to_pin.net.as_ref().is_some_and(|n| n.0 == from_net_id.0);
        if same_net && !include_topology {
            return Self::path_between_envelope(from, to, true, Some(0), Vec::new(), None, None, None);
        }

        let from_comp = CompId(from_pin.comp.0);
        let to_comp = to_pin.comp.0;
        let to_canonical = self.pin_name(to_pin_id);

        // Same net, and it is a rail: the walk would expand the rail and list
        // its whole fanout. The subgraph is just the two parts on the rail.
        if same_net && self.rail_score(self.net(&from_net_id)).0 >= RAIL_THRESHOLD {
            let topology = self.build_topology(
                &[(from_net_id.0, NetState::Rail)],
                vec![(from_comp.0, "start"), (to_comp, "endpoint")],
                &[from_net_id.0],
                max_topology_parts.max(2) as usize,
            );
            return Self::path_between_envelope(
                from, to, true, Some(0), Vec::new(), None, None, Some(topology),
            );
        }

        // max_endpoints is effectively unbounded here: we need to search the
        // whole reachable set for `to`, not stop at the first handful.
        let data =
            self.walk_bfs(&from_net_id, Some(&from_comp), 6, 100_000, true, include_topology);
        let topology = |to_net: usize| {
            include_topology.then(|| {
                self.path_topology(
                    &data,
                    from_net_id.0,
                    to_net,
                    from_comp.0,
                    to_comp,
                    max_topology_parts.max(2) as usize,
                )
            })
        };

        if same_net {
            return Self::path_between_envelope(
                from,
                to,
                true,
                Some(0),
                Vec::new(),
                None,
                None,
                topology(from_net_id.0),
            );
        }

        if let Some(ep) = data.endpoints.iter().find(|e| e.pin == to_canonical) {
            let to_net = to_pin.net.as_ref().expect("a reached pin has a net").0;
            return Self::path_between_envelope(
                from,
                to,
                true,
                Some(ep.distance),
                Self::clone_via(&ep.via),
                None,
                None,
                topology(to_net),
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
                    None,
                    topology(to_net_id.0),
                );
            }
        }

        let diagnosis = self.path_diagnosis(to_pin_id, &data);
        Self::path_between_envelope(
            from,
            to,
            false,
            None,
            Vec::new(),
            Some("no passthrough path within depth 6 (rails are not crossed)".to_string()),
            Some(diagnosis),
            None,
        )
    }

    /// The `path_between` topology (issue #39): `walk`'s graph from `from`,
    /// pruned to the from→to subgraph by `path_subgraph`. It keeps the series
    /// parts on some route from `from_net` to `to_net`, the passive branches
    /// from a route net down to a rail (shunts), and every other endpoint
    /// part with a pin on a route or shunt net, so the part an RC filter
    /// feeds is drawn with it; branches that reach neither `to` nor
    /// a rail are left out. A passive end part counts as an edge for the
    /// route test, but keeps its start or endpoint role in the graph.
    ///
    /// Keep order for the cap: the `from` part, the `to` part, route parts,
    /// shunt parts (each by hop distance), then the other endpoint parts by
    /// distance and natural refdes. `omitted_*` count only what the cap cut,
    /// not what the pruning left out.
    fn path_topology(
        &self,
        data: &WalkData,
        from_net: usize,
        to_net: usize,
        from_comp: usize,
        to_comp: usize,
        max_parts: usize,
    ) -> Topology {
        let topo = &data.topo;
        let in_state = |want: NetState| -> HashSet<usize> {
            topo.nets.iter().filter(|(_, st)| *st == want).map(|(n, _)| *n).collect()
        };
        let walked = in_state(NetState::Expanded);
        let rails = in_state(NetState::Rail);

        let part_nets: Vec<Option<(usize, usize)>> = topo
            .series
            .iter()
            .map(|&(c, _)| {
                // The end parts stay in: a passive `to` part (a cap into a
                // rail) is itself the last route edge. `build_topology`
                // keeps their first role, start or endpoint.
                let nets: Vec<Option<usize>> = self
                    .component(&CompId(c))
                    .pins
                    .iter()
                    .map(|p| self.pin(p).net.as_ref().map(|n| n.0))
                    .collect();
                match nets[..] {
                    [Some(a), Some(b)] => Some((a, b)),
                    _ => None,
                }
            })
            .collect();
        let (roles, route_nets) = path_subgraph(&part_nets, &walked, &rails, from_net, to_net);

        let mut kept_nets = route_nets.clone();
        let mut route_parts: Vec<(u32, usize)> = Vec::new();
        let mut shunt_parts: Vec<(u32, usize)> = Vec::new();
        for (i, &(c, d)) in topo.series.iter().enumerate() {
            let Some((a, b)) = part_nets[i] else { continue };
            match roles[i] {
                PathRole::Route => route_parts.push((d, c)),
                PathRole::Shunt => shunt_parts.push((d, c)),
                PathRole::Off => continue,
            }
            kept_nets.extend([a, b]);
        }
        // Stable: BFS order within a distance.
        route_parts.sort_by_key(|&(d, _)| d);
        shunt_parts.sort_by_key(|&(d, _)| d);

        let mut others: Vec<(usize, u32)> = data
            .endpoints
            .iter()
            .filter(|e| e.comp_idx != from_comp && e.comp_idx != to_comp)
            .filter(|e| kept_nets.contains(&e.net_idx))
            .map(|e| (e.comp_idx, e.distance))
            .collect();
        others.sort_by_cached_key(|&(c, d)| {
            (d, Self::pin_sort_key(&self.component(&CompId(c)).refdes), c)
        });

        let order: Vec<(usize, &'static str)> = [(from_comp, "start"), (to_comp, "endpoint")]
            .into_iter()
            .chain(route_parts.into_iter().map(|(_, c)| (c, "series")))
            .chain(shunt_parts.into_iter().map(|(_, c)| (c, "series")))
            .chain(others.into_iter().map(|(c, _)| (c, "endpoint")))
            .collect();
        let nets: Vec<(usize, NetState)> =
            topo.nets.iter().copied().filter(|(n, _)| kept_nets.contains(n)).collect();
        self.build_topology(&nets, order, &[from_net, to_net], max_parts)
    }

    fn clone_via(via: &[ViaPart]) -> Vec<ViaPart> {
        via.iter()
            .map(|v| ViaPart {
                refdes: v.refdes.clone(),
                value: v.value.clone(),
                class: v.class.clone(),
                dnp: v.dnp,
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
        diagnosis: Option<PathDiagnosis>,
        topology: Option<Topology>,
    ) -> anyhow::Result<String> {
        let envelope = PathBetweenEnvelope {
            from: from.to_string(),
            to: to.to_string(),
            connected,
            distance,
            via,
            note,
            diagnosis,
            topology,
        };
        Ok(serde_json::to_string_pretty(&envelope).context("error serializing path_between")?)
    }

    /// Build the `diagnosis` attached to a negative `path_between` result:
    /// the boundary of `from`'s reachable region, read off the `WalkData`
    /// `path_between` already computed from `from` (no second walk). Purely
    /// descriptive of what `from` reaches — never a suggestion that `to`
    /// should be joined to it.
    fn path_diagnosis(&self, to_pin_id: &PinId, data: &WalkData) -> PathDiagnosis {
        let to_pin = self.pin(to_pin_id);
        let to_net = to_pin.net.as_ref().map(|n| self.net(n).name.clone());

        let sample = data
            .endpoints
            .iter()
            .take(12)
            .map(|e| DiagnosisReach {
                pin: e.pin.clone(),
                kind: e.kind.clone(),
            })
            .collect();

        PathDiagnosis {
            to_net,
            from_reachable_nets: data.reached_net_count,
            from_reaches: FromReaches {
                count: data.endpoints.len(),
                sample,
            },
            // One name per rail: `rails_reached` has an entry per arrival.
            from_rails: {
                let mut seen = std::collections::HashSet::new();
                data.rails_reached
                    .iter()
                    .filter(|r| seen.insert(r.net.as_str()))
                    .map(|r| r.net.clone())
                    .collect()
            },
        }
    }

    pub fn from_netlist(netlist: netlist::Netlist) -> anyhow::Result<Design> {
        let sheet_paths = sheet_path_set(
            netlist.sheets.iter().map(String::as_str).chain(
                netlist.components.iter().filter_map(|c| c.sheet.as_deref()),
            ),
        );

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
            let value_norm = normalize_value(&netlist_comp.value, &Self::refdes_class(&netlist_comp.refdes));
            // Lift the two presence-only flags into typed booleans and drop the
            // keys, so `properties` never carries an entry whose null value
            // would read as "not DNP" when it means the opposite.
            let mut properties = netlist_comp.properties;
            let dnp = presence_flag(&properties, "dnp");
            let exclude_from_bom = presence_flag(&properties, "exclude_from_bom");
            properties.remove("dnp");
            properties.remove("exclude_from_bom");
            let mut comp = Component {
                id: CompId(i),
                refdes: netlist_comp.refdes,
                value: netlist_comp.value,
                value_norm,
                footprint: netlist_comp.footprint,
                description: netlist_comp.description,
                sheet: netlist_comp.sheet,
                dnp,
                exclude_from_bom,
                properties,
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
            net_map: net_map,
            sheet_paths
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

/// One sheet path in the form `sheet_path_set` stores and `net_hierarchy_in`
/// looks up: lowercased (KiCad carries the sheet name's case in both the
/// design header and the net name, but nothing guarantees the two agree),
/// rooted, and trailing-slashed. The two sides arrive in different shapes —
/// "/Power/" from the export, "Power" from a joined run of net-name segments
/// — and normalizing makes them one key.
fn normalize_sheet_path(path: &str) -> String {
    format!("/{}/", path.trim().trim_matches('/').to_lowercase())
}

/// The set of sheet paths a design has, for `net_hierarchy_in` to split net
/// names against.
///
/// Fed from the export's `(design ...)` header *and* the components'
/// sheetpaths: the header is authoritative but older exports omit it, while
/// the components cover only sheets that hold parts. Either source alone
/// leaves a real sheet out, and a missing sheet costs a net its hierarchy.
///
/// Ancestors are added for the same reason: a sheet whose children hold every
/// part ("/Power/Aux/") is still a sheet, and nets can be labelled on it.
fn sheet_path_set<'a>(sheets: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let mut paths: HashSet<String> = HashSet::new();
    for sheet in sheets {
        let segments: Vec<&str> = sheet.split('/').filter(|s| !s.is_empty()).collect();
        for k in 1..=segments.len() {
            paths.insert(normalize_sheet_path(&segments[..k].join("/")));
        }
    }
    // The root sheet is every design's, and its path normalizes to "/" — a
    // name no net-name segment can produce, so it is inert as a split point
    // and present only so the set is not a lie about the design.
    paths.insert("/".to_string());
    paths
}

/// Display name for a sheet path: the path with its rooting slashes trimmed,
/// so "/Power/" reads as "Power".
///
/// The root sheet trims away to nothing, so it keeps its "/". An empty name is
/// not merely unreadable — it is the one string `SubsystemFilter::parse` reads
/// as "no filter", so an agent copying it out of `list_subsystems` or
/// `design_overview` into a `subsystem` argument would get the whole design
/// back: issue #17 again, by way of the discovery path. Both names round-trip
/// as selectors: "Power" as a substring, "/" as the root sheet.
///
/// The `(unassigned)` bucket its callers emit alongside these is the one label
/// that is not a selector — it parses to a substring no sheet contains, so it
/// selects nothing. That is the honest answer for parts KiCad put on no sheet
/// (`SubsystemFilter::matches` refuses them by design), not a silent
/// whole-design match, so it is left as a label.
fn subsystem_display_name(path: &str) -> String {
    let name = path.trim_matches('/');
    if name.is_empty() { "/".to_string() } else { name.to_string() }
}

/// How a `subsystem` argument is matched against a component's sheet path.
///
/// KiCad writes sheet paths rooted and trailing-slashed ("/", "/Power/",
/// "/Power/Aux/"), so a plain substring match makes the root sheet "/"
/// unaddressable: it is a substring of every other path, and selecting it
/// silently returns the whole design (issue #17). The argument's shape picks
/// the matcher:
///
/// - `"/"` — the root sheet and nothing below it. Root-plus-descendants is
///   already what omitting the filter does, so the exact reading is the only
///   useful one.
/// - `"/Power"`, `"/Power/"` — a rooted path: that sheet and any sheet under it.
///   A rooted path no sheet in the design answers to widens to a rooted
///   *prefix*, still anchored at the root — see `widen_if_unmatched`.
/// - `"Power"`, `"sensor"` — a bare name: case-insensitive substring, so
///   "sensor" still spans /Sensor1/../Sensor3/.
#[derive(Debug, PartialEq)]
enum SubsystemFilter {
    /// The root sheet, and only it.
    Root,
    /// A rooted path, lowercased and trailing-slashed ("/power/"): the sheet
    /// itself or anything below it.
    Path(String),
    /// A rooted path that named no sheet, lowercased with its trailing slash
    /// dropped ("/adc"): any sheet whose path starts with it, so "/adc" spans
    /// /ADC1/ and /ADC2/ without reaching /Power/. Only `widen_if_unmatched`
    /// builds this — no argument shape parses to it directly.
    PathPrefix(String),
    /// A bare name, lowercased and slash-trimmed: substring of the sheet path.
    Substring(String),
}

impl SubsystemFilter {
    /// `None` means "no filter" — the argument was absent or blank. Never
    /// "match nothing".
    fn parse(arg: Option<&str>) -> Option<Self> {
        let raw = arg.map(str::trim).filter(|a| !a.is_empty())?;
        let lc = raw.to_lowercase();

        if !lc.starts_with('/') {
            // Non-empty and not slash-led, so trimming cannot empty it.
            return Some(SubsystemFilter::Substring(lc.trim_matches('/').to_string()));
        }
        if lc.chars().all(|c| c == '/') {
            return Some(SubsystemFilter::Root);
        }

        let mut path = lc;
        if !path.ends_with('/') {
            path.push('/');
        }
        Some(SubsystemFilter::Path(path))
    }

    /// Widen a rooted path that no sheet in the design answers to into a
    /// rooted *prefix* on the same string.
    ///
    /// `list_subsystems` hands out rooted paths ("/ADC1/", "/ADC2/"), so the
    /// obvious way to ask for both is their shared prefix "/ADC" — which as a
    /// path names a sheet that does not exist. Without this it returns an
    /// empty page, indistinguishable from "this design has no ADC parts".
    ///
    /// The fallback stays anchored at the root, because an unanchored
    /// substring is issue #17 again: "/E" matches no sheet, and as a substring
    /// it selects "/Power/", "/Ethernet/" and "/Sensor1/" alike — a rooted
    /// argument silently answering with the whole design. As a prefix it
    /// selects "/Ethernet/" and nothing else. A caller who wants the
    /// unanchored reading has one: drop the leading slash.
    ///
    /// Only `Path` widens. A `Root` filter matching nothing means the design
    /// has no parts on its root sheet, which is a true empty answer; widening
    /// it would return the whole design instead.
    fn widen_if_unmatched<'a>(self, mut sheets: impl Iterator<Item = Option<&'a str>>) -> Self {
        if let SubsystemFilter::Path(path) = &self {
            if !sheets.any(|sheet| self.matches(sheet)) {
                return SubsystemFilter::PathPrefix(path.trim_end_matches('/').to_string());
            }
        }
        self
    }

    /// Test one component's sheet (`None` for a component KiCad left
    /// unassigned, which no filter matches).
    fn matches(&self, sheet: Option<&str>) -> bool {
        let sheet_lc = sheet.unwrap_or("").trim().to_lowercase();
        if sheet_lc.is_empty() {
            return false;
        }
        match self {
            SubsystemFilter::Root => sheet_lc == "/",
            SubsystemFilter::Path(path) => {
                // Normalize the sheet the same way the argument was, so
                // "/power" and "/power/" describe the same sheet.
                let sheet_path = if sheet_lc.ends_with('/') {
                    sheet_lc
                } else {
                    format!("{sheet_lc}/")
                };
                sheet_path.starts_with(path.as_str())
            }
            // No trailing slash on either side: "/adc" reaches "/adc1/", which
            // the trailing-slashed Path reading deliberately does not.
            SubsystemFilter::PathPrefix(prefix) => sheet_lc.starts_with(prefix.as_str()),
            SubsystemFilter::Substring(term) => {
                sheet_lc.trim_matches('/').contains(term.as_str())
            }
        }
    }
}

/// The `get_component` output envelope: full identity/properties plus a
/// paginated, structured pin list.
#[derive(Debug, Serialize)]
struct ComponentDetail {
    refdes: String,
    value: String,
    value_norm: Option<ValueNorm>,
    description: Option<String>,
    keywords: Option<String>,
    footprint: Option<String>,
    sheet: Option<String>,
    /// Do Not Populate: the part is on the schematic but deliberately not
    /// fitted. Always emitted, never null — see `presence_flag`.
    dnp: bool,
    /// Excluded from the BOM. Independent of `dnp`.
    exclude_from_bom: bool,
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
    /// True if the owning component is Do Not Populate — the pin is on the
    /// net in the schematic, but not on the built board.
    dnp: bool,
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
    /// Strictly ICs — a net landing on a jack, magnetics or a crystal has a
    /// real endpoint but still reports 0 here; see `endpoint_pin_count`.
    pub ic_pin_count: usize,
    /// Count of pins on parts this net can actually TERMINATE on — ICs,
    /// connectors, and everything that is neither a two-terminal passive nor a
    /// probe/mechanical part (magnetics, crystals, transistors, diodes,
    /// switches, multi-terminal passives like common-mode chokes, ...). See
    /// `is_endpoint_class`. 0 means the net only reaches two-terminal passives
    /// and probe/mechanical parts.
    pub endpoint_pin_count: usize,
    /// True if every pin's owning component is a two-terminal passive
    /// (R/L/C/FB with exactly 2 pins) — i.e. nothing this net could terminate
    /// on touches it. A multi-terminal passive (a common-mode choke, a
    /// resistor network) is an endpoint, not a passive, and clears this.
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
    /// Pins on parts a net can actually terminate on (`is_endpoint_class`).
    /// Cuts across `ic`/`connector`/`other` — it is NOT a fifth disjoint
    /// bucket.
    endpoint: usize,
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
    /// Do Not Populate — the owning part is not fitted on the built board.
    dnp: bool,
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
    value_norm: Option<ValueNorm>,
    description: Option<String>,
    footprint: Option<String>,
    sheet: Option<String>,
    keywords: Option<String>,
    /// Do Not Populate — see `ComponentDetail::dnp`.
    dnp: bool,
    pin_count: usize,
}

#[derive(Debug, Serialize)]
struct FilterEnvelope {
    total: usize,
    offset: u32,
    limit: u32,
    returned: usize,
    /// Present only when the `subsystem` argument was widened — see
    /// `Design::subsystem_filter`.
    #[serde(skip_serializing_if = "Option::is_none")]
    subsystem_note: Option<String>,
    rows: Vec<FilterRow>,
}

/// One neighbor in a `neighbors` net group: the other component on the shared
/// net, plus which of its pins carries it.
#[derive(Debug, Serialize)]
struct NeighborRow {
    refdes: String,
    value: String,
    pin: String,
    /// Do Not Populate — this neighbor is not fitted on the built board.
    dnp: bool,
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
    /// Do Not Populate — the queried part itself is not fitted on the built board.
    dnp: bool,
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
    /// Present only under a `subsystem` filter, which is the only page that
    /// ranks by it (see `Design::filter_nets`). Same score as `get_net`'s,
    /// without the evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    rail_score: Option<f32>,
}

#[derive(Debug, Serialize)]
struct NetEnvelope {
    total: usize,
    offset: u32,
    limit: u32,
    returned: usize,
    /// Present only when the `subsystem` argument was widened — see
    /// `Design::subsystem_filter`.
    #[serde(skip_serializing_if = "Option::is_none")]
    subsystem_note: Option<String>,
    /// Present only when a `subsystem` filter demoted at least one rail to the
    /// end of the ordering — see `Design::filter_nets`.
    #[serde(skip_serializing_if = "Option::is_none")]
    rail_note: Option<String>,
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
    /// Components the schematic marks Do Not Populate. They are still present
    /// in every net and every walk — check the `dnp` flag on a part before
    /// concluding a connection exists on the built board.
    dnp_components: usize,
    /// Components excluded from the BOM (a separate KiCad flag from `dnp`).
    bom_excluded_components: usize,
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

/// One connector (refdes class J/P/CN/RJ/USB) in `design_overview`.
#[derive(Debug, Serialize)]
struct ConnectorRow {
    refdes: String,
    value: String,
    pin_count: usize,
    /// Do Not Populate — this connector is not fitted on the built board.
    dnp: bool,
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
    dead_ends: Vec<DeadEnd>,
    truncated: bool,
    /// Count of distinct nets `walk_bfs` visited (its `visited_nets` set size)
    /// — the size of the connected component reached from the start, used by
    /// `path_between`'s negative-result diagnosis. Does not affect walk/BFS
    /// behavior.
    reached_net_count: usize,
    /// What `build_topology` needs, recorded during the BFS.
    topo: TopoTrace,
}

/// How a net in the walk topology was handled by the BFS.
#[derive(Debug, Clone, Copy, PartialEq)]
enum NetState {
    /// Popped and its pins walked.
    Expanded,
    /// Popped and stopped at: a power/ground rail (`stop_at_power`).
    Rail,
    /// Popped and stopped at: fanout > 40.
    LargeNet,
    /// The far net of a series part at `max_depth`; never popped.
    DepthLimit,
    /// The far net of a series part, with fanout 1; never popped.
    Dangling,
}

impl NetState {
    fn as_str(self) -> &'static str {
        match self {
            NetState::Expanded => "expanded",
            NetState::Rail => "rail",
            NetState::LargeNet => "large_net",
            NetState::DepthLimit => "depth_limit",
            NetState::Dangling => "dangling",
        }
    }

    /// Whether the BFS actually popped the net. A popped state is final; a
    /// non-popped one is replaced if the net is popped later from another
    /// branch.
    fn popped(self) -> bool {
        matches!(self, NetState::Expanded | NetState::Rail | NetState::LargeNet)
    }
}

/// Nets (in first-seen order, with state), series parts (in traversal
/// order) and endpoint parts past `max_endpoints` recorded by `walk_bfs` for
/// `build_topology`.
/// When `record` is false (plain `walk`, `path_between`) every method is a
/// no-op, so the default path pays nothing.
#[derive(Debug, Default)]
struct TopoTrace {
    record: bool,
    nets: Vec<(usize, NetState)>,
    /// net index -> position in `nets`.
    net_pos: HashMap<usize, usize>,
    /// (series part, depth of the net it was traversed from).
    series: Vec<(usize, u32)>,
    /// (endpoint part, depth) for endpoint pins past `max_endpoints`, in
    /// BFS order. A part can appear more than once.
    dropped_endpoints: Vec<(usize, u32)>,
}

impl TopoTrace {
    fn new(record: bool) -> Self {
        TopoTrace { record, ..Default::default() }
    }

    fn note_net(&mut self, net_idx: usize, state: NetState) {
        if !self.record {
            return;
        }
        match self.net_pos.get(&net_idx) {
            None => {
                self.net_pos.insert(net_idx, self.nets.len());
                self.nets.push((net_idx, state));
            }
            Some(&i) => {
                let old = &mut self.nets[i].1;
                if !old.popped() && state.popped() {
                    *old = state;
                }
            }
        }
    }

    fn note_series(&mut self, comp_idx: usize, depth: u32) {
        if self.record {
            self.series.push((comp_idx, depth));
        }
    }

    fn note_dropped_endpoint(&mut self, comp_idx: usize, depth: u32) {
        if self.record {
            self.dropped_endpoints.push((comp_idx, depth));
        }
    }
}

/// For each edge of an undirected multigraph, whether it lies on some simple
/// path from `s` to `t`. Nodes are any `usize`; a self-loop is never on one,
/// and nothing is when `s == t`.
///
/// Add a virtual edge s–t. An edge lies on a simple s–t path exactly when it
/// sits in the same block (biconnected component) as that virtual edge: the
/// path plus the virtual edge is a simple cycle, and any two edges of one
/// block lie on a common simple cycle. The blocks come from Tarjan's
/// algorithm, run iteratively and keyed by edge id so parallel edges count.
fn edges_on_st_paths(edges: &[(usize, usize)], s: usize, t: usize) -> Vec<bool> {
    let mut on = vec![false; edges.len()];
    if s == t {
        return on;
    }
    let mut ids: HashMap<usize, usize> = HashMap::new();
    let mut dense = |n: usize| {
        let next = ids.len();
        *ids.entry(n).or_insert(next)
    };
    let mut es: Vec<(usize, usize)> = edges.iter().map(|&(a, b)| (dense(a), dense(b))).collect();
    let (ds, dt) = (dense(s), dense(t));
    let virt = es.len();
    es.push((ds, dt));
    let n = ids.len();
    let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (e, &(a, b)) in es.iter().enumerate() {
        if a != b {
            adj[a].push((b, e));
            adj[b].push((a, e));
        }
    }

    const NONE: usize = usize::MAX;
    let mut disc = vec![NONE; n];
    let mut low = vec![0; n];
    let mut timer = 1;
    let mut edge_stack: Vec<usize> = Vec::new();
    // (node, edge it was entered by, next adjacency index)
    let mut stack: Vec<(usize, usize, usize)> = vec![(ds, NONE, 0)];
    disc[ds] = 0;
    while let Some(&(u, entered_by, i)) = stack.last() {
        if i < adj[u].len() {
            stack.last_mut().expect("non-empty").2 += 1;
            let (v, e) = adj[u][i];
            if e == entered_by {
                continue;
            }
            if disc[v] == NONE {
                edge_stack.push(e);
                disc[v] = timer;
                low[v] = timer;
                timer += 1;
                stack.push((v, e, 0));
            } else if disc[v] < disc[u] {
                // Back edge (or a parallel edge to the parent).
                edge_stack.push(e);
                low[u] = low[u].min(disc[v]);
            }
            continue;
        }
        stack.pop();
        let Some(&(p, _, _)) = stack.last() else { break };
        low[p] = low[p].min(low[u]);
        if low[u] >= disc[p] {
            // `p` closes a block: the edges pushed since `entered_by`.
            let mut block = Vec::new();
            while let Some(e) = edge_stack.pop() {
                block.push(e);
                if e == entered_by {
                    break;
                }
            }
            if block.contains(&virt) {
                for e in block.into_iter().filter(|&e| e != virt) {
                    on[e] = true;
                }
                return on;
            }
        }
    }
    on
}

/// What the `path_between` topology does with one series part (issue #39).
#[derive(Debug, Clone, Copy, PartialEq)]
enum PathRole {
    /// On some route from the `from` net to the `to` net.
    Route,
    /// Off the route, on a passive branch from a route net to a rail.
    Shunt,
    /// Neither: a branch that leads to neither `to` nor a rail.
    Off,
}

/// Classify series parts for the `path_between` topology (issue #39). Each
/// part is given as its two nets (`None` when a pin has no net). `walked` are
/// the nets the BFS expanded; `rails` the rails it stopped at. `s` and `t`
/// are the `from` and `to` nets; `t` can be a rail (a rail-only path).
///
/// A route part lies on a simple path from `s` to `t` through walked nets.
/// The route nets are `s`, `t` and every net of a route part; they are
/// returned with the roles. A shunt part
/// lies on a simple path from any route net to any rail through walked nets
/// off the route: the route nets are merged into one node and the rails into
/// another, and the same test runs between the two.
fn path_subgraph(
    parts: &[Option<(usize, usize)>],
    walked: &HashSet<usize>,
    rails: &HashSet<usize>,
    s: usize,
    t: usize,
) -> (Vec<PathRole>, HashSet<usize>) {
    // Edges a test leaves out become self-loops, which are never on a path.
    const SELF: (usize, usize) = (usize::MAX, usize::MAX);
    let on_route_graph = |n: usize| n == t || walked.contains(&n);
    let route_edges: Vec<(usize, usize)> = parts
        .iter()
        .map(|p| match *p {
            Some((a, b)) if on_route_graph(a) && on_route_graph(b) => (a, b),
            _ => SELF,
        })
        .collect();
    let route = edges_on_st_paths(&route_edges, s, t);

    let mut route_nets: HashSet<usize> = HashSet::from([s, t]);
    for (i, p) in parts.iter().enumerate() {
        if let (true, Some((a, b))) = (route[i], p) {
            route_nets.extend([*a, *b]);
        }
    }

    const ROUTE: usize = usize::MAX - 1;
    const RAIL: usize = usize::MAX - 2;
    let node = |n: usize| {
        if route_nets.contains(&n) {
            Some(ROUTE)
        } else if rails.contains(&n) {
            Some(RAIL)
        } else {
            walked.contains(&n).then_some(n)
        }
    };
    let shunt_edges: Vec<(usize, usize)> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| match *p {
            Some((a, b)) if !route[i] => match (node(a), node(b)) {
                (Some(a), Some(b)) => (a, b),
                _ => SELF,
            },
            _ => SELF,
        })
        .collect();
    let shunt = edges_on_st_paths(&shunt_edges, ROUTE, RAIL);

    let roles = (0..parts.len())
        .map(|i| {
            if route[i] {
                PathRole::Route
            } else if shunt[i] {
                PathRole::Shunt
            } else {
                PathRole::Off
            }
        })
        .collect();
    (roles, route_nets)
}

/// The `walk` topology graph (`include_topology`): nets are nodes, parts are
/// edges that list the net on each of their pins.
#[derive(Debug, Serialize)]
struct Topology {
    /// True when `max_topology_parts` cut parts from the graph. Separate from
    /// the top-level `truncated`, which the depth and endpoint limits set.
    /// `max_endpoints` does not cut the graph (issue #35), so false means
    /// every part the walk reached is listed. Parts past the depth limit or a
    /// rail were never reached and are not counted.
    truncated: bool,
    /// Parts the walk reached but the cap left out.
    omitted_parts: usize,
    /// Nets the walk reached that no kept part touches, so they are left out.
    omitted_nets: usize,
    nets: Vec<TopoNet>,
    parts: Vec<TopoPart>,
}

#[derive(Debug, Serialize)]
struct TopoNet {
    name: String,
    fanout: usize,
    /// Rail score at or above the rail threshold, whatever `state` is.
    rail: bool,
    /// "expanded", "rail", "large_net", "depth_limit" or "dangling".
    state: &'static str,
    /// Parts with a pin on this net that `max_topology_parts` cut. Above 0,
    /// this node's edges in the graph are incomplete. `max_endpoints` cuts
    /// nothing here (issue #35). Left out when 0. Absent does not mean complete: a node whose `state`
    /// is not "expanded" was never walked past, so its other members are
    /// not listed either way.
    #[serde(skip_serializing_if = "is_zero")]
    omitted_parts: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
struct TopoPart {
    refdes: String,
    value: String,
    class: String,
    dnp: bool,
    /// "start" (the part owning the start pin), "series" (a traversed
    /// passthrough) or "endpoint" (an endpoint part the walk reached, listed
    /// in `endpoints` or not).
    role: &'static str,
    /// Series parts list every pin; start and endpoint parts only the pins on
    /// a net in `nets`.
    pins: Vec<TopoPin>,
}

#[derive(Debug, Serialize)]
struct TopoPin {
    pin: String,
    name: Option<String>,
    /// Null for an unconnected pin.
    net: Option<String>,
}

/// One series part traversed on the way to an endpoint or terminal net.
#[derive(Debug, Serialize)]
struct ViaPart {
    refdes: String,
    value: String,
    class: String,
    /// Do Not Populate. A DNP series part means this hop does **not** exist on
    /// the built board — the path is schematic topology only.
    dnp: bool,
}

/// The owning component of a reached endpoint pin (compact identity only).
#[derive(Debug, Serialize)]
struct EndpointComponent {
    refdes: String,
    value: String,
    description: Option<String>,
    sheet: Option<String>,
    /// Do Not Populate — the endpoint part is not fitted on the built board.
    dnp: bool,
}

/// One opaque endpoint reached by `walk`: the specific pin, its function, the
/// owning component, the series parts traversed (`via`), and hop distance.
#[derive(Debug, Serialize)]
struct WalkEndpoint {
    pin: String,
    pin_name: Option<String>,
    pin_type: Option<String>,
    component: EndpointComponent,
    /// Broad function grouping derived from the endpoint's refdes class:
    /// "ic", "connector", "diode", "transistor", "transformer", "crystal",
    /// "switch", "test_point", or "other".
    kind: String,
    via: Vec<ViaPart>,
    distance: u32,
    /// Owning component, for `build_topology`. Not serialized.
    #[serde(skip)]
    comp_idx: usize,
    /// The net the endpoint pin sits on, for `path_topology`. Not serialized.
    #[serde(skip)]
    net_idx: usize,
}

/// A branch that fizzled out instead of reaching a real endpoint: reached
/// only through at least one passthrough (`via` non-empty, never the start
/// net). Either a passthrough whose far pin is NC or lands on a single-pin
/// net ("dangling"), or a reached net with no endpoint part on it at all
/// ("passive_only" — see `is_endpoint_class`).
#[derive(Debug, Serialize)]
struct DeadEnd {
    net: Option<String>,
    fanout: usize,
    via: Vec<ViaPart>,
    /// The non-endpoint parts found on the net: two-terminal passives plus any
    /// probe/mechanical parts. Probe parts are listed even though they also
    /// appear under `endpoints`, so `reason` never contradicts an unexplained
    /// gap here.
    parts: Vec<ViaPart>,
    reason: String,
}

/// One arrival at a power/ground rail the walk stopped at (reported, never
/// enumerated). A rail reached over several series parts has one entry per
/// arrival, in BFS order (nearest first), so shunt parts to the rail — filter
/// caps, pull-downs, TVS — each show up (issue #31). A repeat arrival is
/// listed only while `rails_reached` is shorter than `max_endpoints`, and a
/// cut one sets `truncated`. The first arrival at each rail is always listed,
/// even past the cap.
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
    dead_ends: Vec<DeadEnd>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    topology: Option<Topology>,
}

/// The `path_between` output envelope. `via` is empty for a direct same-net
/// (or same-pin) connection; `distance` is the passthrough hop count, null
/// when not connected. `diagnosis` is only populated when `connected` is
/// false and a `from`-side walk was actually performed.
#[derive(Debug, Serialize)]
struct PathBetweenEnvelope {
    from: String,
    to: String,
    connected: bool,
    distance: Option<u32>,
    via: Vec<ViaPart>,
    note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnosis: Option<PathDiagnosis>,
    /// With `include_topology`, when connected: the from→to subgraph.
    #[serde(skip_serializing_if = "Option::is_none")]
    topology: Option<Topology>,
}

/// One endpoint in a `path_diagnosis` reachability sample: just enough to
/// place it (pin + broad function kind), matching `WalkEndpoint::kind`.
#[derive(Debug, Serialize)]
struct DiagnosisReach {
    pin: String,
    kind: String,
}

/// Capped sample of the endpoints `from` can reach, plus the true total.
#[derive(Debug, Serialize)]
struct FromReaches {
    count: usize,
    sample: Vec<DiagnosisReach>,
}

/// Factual description of the boundary of `from`'s reachable region, attached
/// to a negative `path_between` result so the caller can see where `from`'s
/// connectivity actually stops instead of a bare `false`. Never states or
/// implies that nets *should* be joined — purely a report of what `from`
/// already reaches.
#[derive(Debug, Serialize)]
struct PathDiagnosis {
    /// The net `to` sits on, or null if `to` pin has no net at all.
    to_net: Option<String>,
    /// Distinct nets in `from`'s connected component (the BFS's visited-net
    /// count).
    from_reachable_nets: usize,
    from_reaches: FromReaches,
    /// Power/ground rail net names `from`'s walk reached.
    from_rails: Vec<String>,
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
// The score at or above which a net is treated as a rail rather than a signal:
// `design_overview` lists it as a detected rail, `walk` stops at it instead of
// enumerating it, and `filter_nets` sorts it last under a subsystem filter.
const RAIL_THRESHOLD: f32 = 0.5;
// How many demoted rails `filter_nets` names in its `rail_note` before falling
// back to a count; the note is an orientation aid, not a second result set.
const RAIL_NOTE_NAMES: usize = 5;

// Pin-type and refdes-class sets shared by `Design::net_role` and
// `Design::audit`. A "driver" is any pin type capable of actively asserting
// a level onto the net; `power_out` counts as both a driver and the sole
// "source" type.
const DRIVER_PIN_TYPES: &[&str] = &[
    "output", "bidirectional", "tri_state", "open_collector", "open_emitter", "power_out",
];
const PASSIVE_CLASSES: &[&str] = &["R", "L", "C", "FB"];
const CONNECTOR_CLASSES: &[&str] = &["J", "P", "CN", "RJ", "USB"];
const IC_CLASS: &str = "U";
// Parts that touch a net without terminating it: probe points and mechanical
// hardware. Together with PASSIVE_CLASSES these are the ONLY classes
// `is_endpoint_class` treats as non-endpoints.
//
// Add to this list only for a prefix that is UNAMBIGUOUSLY inert, and check it
// against the stock KiCad libraries first: a wrong entry here is silent and
// unbounded, since every part in the class then vanishes from stub detection
// and from walk endpoints. "MK" looked mechanical and is not — KiCad gives it
// to microphones (Device: Microphone*, all of Sensor_Audio: ICS-43434,
// SPH0645LM4H, IM69D130, ...), which are exactly the kind of endpoint #13 was
// about. "H" (mounting holes/screws/outlines), "FID" (fiducials) and "TP"
// (test points) are inert throughout those libraries, with one exception worth
// knowing: Connector's `CoaxialSwitch_Testpoint` also takes "TP" and is a
// 3-pin RF switch sitting IN the signal path. "MH" has no owner in the stock
// libraries at all — it is included by convention (mounting hole), not by
// verification.
const PROBE_CLASSES: &[&str] = &["TP", "H", "MH", "FID"];

/// Is this component the two-terminal passive a walk passes THROUGH rather
/// than stopping at? Class alone is not enough: `L` is a passthrough as a
/// 2-pin inductor and a real endpoint as a 4-pin common-mode choke, and the
/// same holds for 3-terminal EMI filters and resistor networks carrying an
/// `R` prefix.
///
/// This is THE definition of a passthrough — `walk` branches on it, and
/// `is_endpoint_class` is its complement — so the two can never disagree
/// about a part.
fn is_passive_passthrough(class: &str, pin_count: usize) -> bool {
    PASSIVE_CLASSES.contains(&class) && pin_count == 2
}

/// Can a net actually TERMINATE on this component?
///
/// Deliberately a deny-list, not an allow-list: an allow-list of "real" parts
/// is never finished (RJ45 jacks, magnetics, crystals, relays, opto-isolators,
/// modules, antennas, ... all keep arriving), and every class it has not heard
/// of gets silently reported as a dead end. The parts that are genuinely NOT
/// endpoints are a short, closed set — two-terminal passives, which a walk
/// passes THROUGH rather than stopping at, and probe/mechanical parts, which
/// merely touch a net. Everything else is assumed to be a real part.
///
/// `pin_count` is the whole reason this takes more than a class: a passive
/// prefix earns its exemption only at exactly two terminals. Anything else
/// wearing an `R`/`L`/`C`/`FB` prefix — a choke, a filter, a network, or a
/// 1-pin passive that should not exist — is reported as the endpoint it is,
/// which is also what `walk` already did with it.
///
/// `class` must come from `Design::refdes_class` (uppercased leading
/// non-digit prefix); `pin_count` from `Component::pins`.
pub fn is_endpoint_class(class: &str, pin_count: usize) -> bool {
    !is_passive_passthrough(class, pin_count) && !PROBE_CLASSES.contains(&class)
}

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

/// Read a KiCad presence-only symbol flag (`dnp`, `exclude_from_bom`) out of a
/// component's property map.
///
/// The netlist exporter emits these as a *name with no value* — `(property
/// (name "dnp"))` — and emits them **only for the parts that carry the flag**.
/// So the key's presence is the flag; the `None` value is not "unknown", it is
/// how KiCad spells `true`. A value is still honoured if some other exporter
/// writes one, with the usual falsy spellings rejected.
fn presence_flag(properties: &HashMap<String, Option<String>>, key: &str) -> bool {
    match properties.get(key) {
        None => false,
        Some(None) => true,
        Some(Some(v)) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "no" | "false" | "0"
        ),
    }
}

#[derive(Debug)]
pub struct Component {
    id: CompId,
    refdes: String,
    value: String,
    value_norm: Option<ValueNorm>,
    footprint: Option<String>,
    description: Option<String>,
    sheet: Option<String>,
    /// True if the schematic marks this part Do Not Populate. Derived from the
    /// netlist's presence-only `dnp` property (see `presence_flag`) and lifted
    /// out of `properties` so it can never be read as a null/unknown.
    dnp: bool,
    /// True if the schematic excludes this part from the BOM. A *separate*
    /// KiCad flag from `dnp`: a part can be DNP and still in the BOM, or in
    /// the BOM and excluded from it.
    exclude_from_bom: bool,
    properties: HashMap<String, Option<String>>,
    pins: Vec<PinId>
}

/// Best-effort normalization of a passive's raw `value` string into a base-unit
/// magnitude plus a canonical display form. `value` stays authoritative — this
/// is a derived convenience field so callers can group/count/compare passives
/// (e.g. "33 pF" and "33p") without reparsing free text themselves.
#[derive(Debug, Serialize, Clone)]
pub struct ValueNorm {
    /// The value in base SI units (ohms for R and FB, farads for C, henries
    /// for L).
    pub magnitude: f64,
    /// The base unit symbol: "Ω", "F", or "H".
    pub unit: String,
    /// A normalized display string using the SI prefix that puts the
    /// mantissa in [1, 1000), e.g. "348kΩ", "33pF", "4.7µF", "0Ω". A bead's
    /// carries its frequency too ("1kΩ@100MHz"), since that is part of what
    /// makes two beads the same part — see `at_frequency`.
    pub canonical: String,
    /// The frequency the magnitude is specified at, copied verbatim from the
    /// value string ("100MHz"). Only ferrite beads have one: a bead's rating
    /// is an impedance at a stated frequency, and 1kΩ@100MHz and 1kΩ@10MHz
    /// are different parts. Omitted from the JSON when absent, so R/C/L are
    /// serialized exactly as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_frequency: Option<String>,
}

/// SI prefix letter -> multiplier, for a value of the given class.
///
/// Case-tolerant exactly where the domain leaves no ambiguity: 'K', 'P', 'N'
/// and 'U' name no other quantity a passive is written in, so "24K" is 24k and
/// "33P" is 33p (issue #18). 'G' keeps its case, since a lowercase giga is
/// never written and folding it buys nothing.
///
/// 'M' is the one prefix whose reading depends on the class, because that is
/// where the ambiguity does or does not exist:
///
/// - **R**: strict. Milli and mega are both real for a resistor and case is
///   all that separates a 1mΩ shunt from a 1MΩ bleeder, so folding would turn
///   one into the other.
/// - **L**: milli. There is no megahenry, so uppercase "10MH" can only mean
///   10mH — the same no-other-quantity argument that folds 'K' and 'P'.
/// - **C**: refused, by the range rule below. Neither reading is safe: the
///   megafarad does not exist, but legacy US notation spells the *micro*farad
///   "MF"/"MFD", so uppercase 'M' on a capacitor is genuinely ambiguous and a
///   null is the honest answer.
///
/// A prefix is refused outright when it names a magnitude the class is not
/// made in, because then the letter is something else — most often a tolerance
/// code, where "104K" is 100nF ±10% and *not* 104kF. Refusing it leaves the
/// honest null these fields had before uppercase folding rather than a number
/// that is confidently off by orders of magnitude. See [`prefix_is_real`].
///
/// Both micro signs are accepted: U+00B5 MICRO SIGN and U+03BC GREEK SMALL
/// LETTER MU, which exports use interchangeably.
fn si_prefix(c: char, unit_letter: char) -> Option<f64> {
    si_multiplier(c, unit_letter).filter(|&mult| prefix_is_real(mult, unit_letter))
}

/// The multiplier an SI prefix letter stands for, whether or not parts of the
/// class come in that decade. [`si_prefix`] adds that check.
fn si_multiplier(c: char, unit_letter: char) -> Option<f64> {
    Some(match c {
        'p' | 'P' => 1e-12,
        'n' | 'N' => 1e-9,
        'u' | 'U' | 'µ' | 'μ' => 1e-6,
        'm' => 1e-3,
        'k' | 'K' => 1e3,
        // Uppercase 'M' is milli for an inductor, mega elsewhere; see above.
        'M' if unit_letter == 'H' => 1e-3,
        'M' => 1e6,
        'G' => 1e9,
        _ => return None,
    })
}

/// Whether parts of this class are made in the decade `mult` names.
///
/// Resistors run from the microohm shunt at the bottom of current sensing up
/// to the gigaohm bias resistor; a nanoohm or picoohm resistor is not a part,
/// so "100 n" on an R is a misread, not a 100nΩ shunt. Capacitance and
/// inductance stop at the part itself — a 10F supercap is real, a kilofarad or
/// kilohenry is not — while their bottom end, the picofarad and the picohenry,
/// is the smallest prefix there is.
fn prefix_is_real(mult: f64, unit_letter: char) -> bool {
    match unit_letter {
        'R' => mult >= 1e-6,
        _ => mult < 1e3,
    }
}

/// True if `c` is the class's own base-unit letter (case-insensitive), or the
/// ohm sign for resistors.
fn is_unit_char(c: char, unit_letter: char) -> bool {
    (unit_letter == 'R' && c == 'Ω') || c.to_ascii_uppercase() == unit_letter
}

/// Strip a trailing long-form unit word ("ohm"/"ohms", case-insensitive) —
/// only resistors spell the unit out as a word in practice.
fn strip_long_unit_word(tok: &str, unit_letter: char) -> &str {
    if unit_letter != 'R' {
        return tok;
    }
    let lower = tok.to_lowercase();
    if lower.ends_with("ohms") {
        &tok[..tok.len() - 4]
    } else if lower.ends_with("ohm") {
        &tok[..tok.len() - 3]
    } else {
        tok
    }
}

/// How many digits may follow a decimal marker. Markings carry at most four
/// significant figures ("1K00", "4R99"), so a longer digit run is not a value
/// at all — it is a part number whose own letter landed where a prefix goes,
/// and "2N7002" must stay unparsed rather than become 2.7002nΩ.
const MAX_DECIMAL_MARKER_DIGITS: usize = 3;

/// Parse one value token ("348k", "4R7", "0.1uF", "10R", "100n") into a
/// base-unit magnitude. Handles a trailing SI-prefix/unit suffix ("100n",
/// "0.1uF") and the decimal-marker style where the prefix or unit letter
/// stands in for the decimal point ("4k7" -> 4700, "4R7" -> 4.7). Returns
/// `None` when the token carries no usable leading digits (e.g. "placeholder",
/// bare "R").
fn parse_value_token(tok: &str, unit_letter: char) -> Option<f64> {
    let tok = strip_long_unit_word(tok, unit_letter);
    if tok.is_empty() {
        return None;
    }
    let chars: Vec<char> = tok.chars().collect();
    let marker_pos =
        chars.iter().position(|&c| si_prefix(c, unit_letter).is_some() || is_unit_char(c, unit_letter));

    let Some(pos) = marker_pos else {
        return tok.parse::<f64>().ok();
    };

    let int_part: String = chars[..pos].iter().collect();
    if int_part.is_empty() {
        return None;
    }
    let marker_mult = si_prefix(chars[pos], unit_letter).unwrap_or(1.0);
    let tail: String = chars[pos + 1..].iter().collect();

    if tail.is_empty() {
        int_part.parse::<f64>().ok().map(|n| n * marker_mult)
    } else if tail.chars().all(|c| c.is_ascii_digit())
        && tail.chars().count() <= MAX_DECIMAL_MARKER_DIGITS
    {
        // Decimal-marker style: the marker itself acts as the decimal point.
        format!("{int_part}.{tail}").parse::<f64>().ok().map(|n| n * marker_mult)
    } else {
        // e.g. "0.1uF": marker 'u' consumed, tail "F" must itself be the unit letter.
        let tail_chars: Vec<char> = tail.chars().collect();
        let tail_is_unit = tail_chars.len() == 1 && is_unit_char(tail_chars[0], unit_letter);
        if tail_is_unit {
            int_part.parse::<f64>().ok().map(|n| n * marker_mult)
        } else {
            None
        }
    }
}

/// A bare unit token with no leading digits, e.g. "pF" in "33 pF" — used only
/// as a fallback when the value's first token is a plain number and a later
/// token spells out the prefix/unit.
///
/// A lone prefix letter counts: "100 k" is 100k, not 100 (issue #18). That
/// case used to fall through to the caller's multiplier of 1, which is worse
/// than the null the issue reports — a wrong magnitude rather than a missing
/// one. It counts only in the token right after the number, which is where a
/// split-off prefix is written — `adjacent` says so. Further out, a single
/// letter is a tolerance or dielectric code that happens to spell a prefix:
/// the K in "4.7 ohm K" is ±10%, not kilo.
fn parse_unit_only_token(tok: &str, unit_letter: char, adjacent: bool) -> Option<f64> {
    let tok = strip_long_unit_word(tok, unit_letter);
    let chars: Vec<char> = tok.chars().collect();
    match chars.len() {
        1 if is_unit_char(chars[0], unit_letter) => Some(1.0),
        1 if adjacent => si_prefix(chars[0], unit_letter),
        2 if is_unit_char(chars[1], unit_letter) => si_prefix(chars[0], unit_letter),
        _ => None,
    }
}

/// Render a base-unit magnitude as a canonical display string using the SI
/// prefix that puts the mantissa in roughly [1, 1000).
fn format_canonical(magnitude: f64, unit_symbol: &str) -> String {
    if magnitude == 0.0 {
        return format!("0{unit_symbol}");
    }
    const PREFIXES: &[(f64, &str)] = &[
        (1e9, "G"), (1e6, "M"), (1e3, "k"), (1.0, ""),
        (1e-3, "m"), (1e-6, "µ"), (1e-9, "n"), (1e-12, "p"),
    ];
    let abs = magnitude.abs();
    let chosen = PREFIXES.iter()
        .find(|(threshold, _)| abs >= *threshold)
        .unwrap_or_else(|| PREFIXES.last().unwrap());
    let mantissa = magnitude / chosen.0;
    let rounded = (mantissa * 1000.0).round() / 1000.0;
    let mantissa_str = if rounded.fract() == 0.0 {
        format!("{}", rounded as i64)
    } else {
        format!("{:.3}", rounded).trim_end_matches('0').trim_end_matches('.').to_string()
    };
    format!("{mantissa_str}{}{unit_symbol}", chosen.1)
}

/// Best-effort parse of a passive's raw `value` into a normalized magnitude.
/// Only meaningful for R/C/L/FB (resistors, capacitors, inductors, ferrite
/// beads); any other `refdes_class` returns `None`.
///
/// For R/C/L the FIRST whitespace-separated token in `value` is taken as the
/// value token; later tokens (ratings like "50V", dielectric codes like
/// "C0G") are ignored for the magnitude, except that when the first token is
/// a bare number, a later token may supply the prefix/unit (e.g. "33 pF").
///
/// FB is ohms too but cannot use that rule — see [`normalize_bead_value`].
pub fn normalize_value(value: &str, refdes_class: &str) -> Option<ValueNorm> {
    match refdes_class {
        "R" => normalize_simple_value(value, "Ω", 'R'),
        "C" => normalize_simple_value(value, "F", 'F'),
        "L" => normalize_simple_value(value, "H", 'H'),
        "FB" => normalize_bead_value(value),
        _ => None,
    }
}

/// The R/C/L reading described on [`normalize_value`]: the first token is the
/// value, later tokens are ratings unless the first was a bare number.
fn normalize_simple_value(value: &str, unit_symbol: &str, unit_letter: char) -> Option<ValueNorm> {
    let mut tokens = value.split_whitespace();
    let first = tokens.next()?;
    let stripped = strip_long_unit_word(first, unit_letter);
    if stripped.is_empty() {
        return None;
    }
    let has_marker =
        stripped.chars().any(|c| si_prefix(c, unit_letter).is_some() || is_unit_char(c, unit_letter));

    let magnitude = if has_marker {
        parse_value_token(first, unit_letter)?
    } else {
        let bare: f64 = stripped.parse().ok()?;
        let mult = tokens
            .enumerate()
            .find_map(|(i, t)| parse_unit_only_token(t, unit_letter, i == 0))
            .unwrap_or(1.0);
        bare * mult
    };

    Some(ValueNorm {
        magnitude,
        unit: unit_symbol.to_string(),
        canonical: format_canonical(magnitude, unit_symbol),
        at_frequency: None,
    })
}

/// The band, in ohms, a ferrite bead's impedance rating lies in. The smallest
/// bead anyone catalogues is a few ohms and the largest a few kilohms, so a
/// number outside it is not an impedance at all but something else the value
/// field happens to hold: a DC resistance under the floor ("120mΩ"), a bare
/// numeric part number over the ceiling ("742792625" reads as 742.793MΩ).
///
/// Bounding the reading is what lets a bead string be read at all. Without it
/// a lone "1.5A 120mΩ" — a real spelling, where the impedance is left to the
/// MPN — is indistinguishable from a lone "1.5A 120Ω", and the DCR gets
/// reported as the value: exactly the confident wrong answer issue #20 is
/// about.
///
/// It is a plausibility bound, not a parser: a DCR of 1.5Ω (the high end for
/// a small-signal bead) sits inside the band unless the string says it was
/// measured at DC, and a short numeric MPN or package code with no leading
/// zero ("2512", "1206") still reads as an ohm value. One with a leading zero
/// ("0603") does not ([`is_package_code`]). What the band cannot rule out,
/// the rules below either leave ambiguous or read as written.
const BEAD_OHMS: std::ops::RangeInclusive<f64> = 1.0..=10_000.0;

/// The FB reading: pick the *impedance*, not the first number that parses.
///
/// A bead is rated as an impedance at a frequency, and its value string
/// normally lists two other ohm-valued quantities before it — the real ones
/// from an 879-component board read `1.5A 120mΩ 1kΩ@100MHz` and
/// `450mA 290mΩ 220Ω@100MHz`: current rating, DC resistance, then the
/// impedance anyone actually means by "the value of the bead". Taking the
/// first parseable token, which is what routing FB through
/// [`normalize_simple_value`] would do, reports the 120mΩ DCR — a confident,
/// plausible, wrong answer where the old `None` was merely incomplete
/// (issue #20).
///
/// Candidates are the tokens that parse as ohms, fall in [`BEAD_OHMS`], and
/// are not marked as measured at DC. Then, in order:
///
/// 1. An ohm token carrying a frequency is the impedance rating, named as
///    such. Use it, and keep the frequency, since a bead's impedance means
///    nothing without it. Quoted points that do not agree — on the ohms or
///    on the frequency — are a spec table rather than a rating: which point
///    is the headline figure is the one thing the string does not say, so
///    return `None` rather than take one by position, which would make the
///    same part read differently depending on the order someone wrote it in.
///    Agreeing on the ohms alone is not enough, since `at_frequency` is part
///    of what makes two beads the same part.
/// 2. Otherwise, a token that *says* it is a resistance ("600R", "1kΩ") beats
///    bare numbers that do not — a package code, the digits of a split
///    rating, a fragment of a part number.
/// 3. Otherwise, if more than one token could be read as the ohms, there is
///    nothing to tell DCR from impedance: return `None` rather than guess.
/// 4. Otherwise it is a plain single-value string ("120 ohm", "1 k"), which
///    reads exactly like a resistor's — read from the candidate's own token,
///    so that a leading rating does not take the value's place ("2A 1k"),
///    and held to the same band.
///
/// What it still gives up, all of it in the direction of a missing frequency
/// rather than a wrong value: a frequency written before its value
/// ("100 MHz 600R") or after a value that does not carry its own unit
/// ("1k 100MHz") is dropped, and punctuation stuck to the *value* rather
/// than the frequency ("1kΩ, 100MHz") defeats [`parse_value_token`] the same
/// way it does for an R or a C.
fn normalize_bead_value(value: &str) -> Option<ValueNorm> {
    let split = bead_split(value);

    // Every token that could be read as a bead impedance, paired with the
    // frequency it was quoted at. Ratings in other units ("1.5A", "25V")
    // parse as nothing, DC resistances fall under the band, and a quantity
    // marked as measured at DC drops out in `bead_tokens` — so none of them
    // makes a string look ambiguous.
    let read = bead_tokens(&split)?;
    let struck_dc = read.iter().any(|tok| tok.dc);
    let candidates: Vec<(BeadToken, f64)> = read
        .into_iter()
        .filter(|tok| !tok.dc)
        .filter_map(|tok| {
            let magnitude = bead_ohms(&tok.base)?;
            Some((tok, magnitude))
        })
        .collect();

    let marked: Vec<&(BeadToken, f64)> =
        candidates.iter().filter(|(tok, _)| tok.freq.is_some()).collect();

    let (magnitude, freq) = match marked.as_slice() {
        // 1. The impedance rating, named as such — unless the quoted points
        //    disagree, which says a table was pasted in, not a value. Marks
        //    are canonicalized, so equality is the right test.
        [first, rest @ ..] => {
            if rest.iter().any(|(tok, magnitude)| {
                *magnitude != first.1 || tok.freq != first.0.freq
            }) {
                return None;
            }
            if is_four_digit_number(&first.0.base) {
                return None;
            }
            (first.1, first.0.freq.clone())
        }
        // No frequency quoted anywhere, so the ohm mark is all there is to go
        // on.
        [] => {
            let ohm_marked: Vec<&(BeadToken, f64)> = candidates
                .iter()
                .filter(|(tok, _)| is_ohm_marked_token(&tok.base))
                .collect();
            match ohm_marked.as_slice() {
                // 2. One token says of itself that it is a resistance and the
                //    others are bare numbers — package codes ("0603"), MPN
                //    fragments. It need not be
                //    the first token: "2A 600R" is still a 600Ω bead.
                [only] => (only.1, None),
                // 3. Several in-band numbers that could each be the ohms, and
                //    nothing saying which is the impedance.
                [] if candidates.len() > 1 => return None,
                // 4. The unit is split off or absent ("120 ohm", "1 k"), and
                //    the value reads exactly like a resistor's — read from
                //    the candidate's own token, since the resistor reading
                //    takes the *first* token for the value and here that may
                //    be a rating ("2A 1k"). With no candidate at all it
                //    starts from the top, unless the top is a package code.
                //    Once a value was struck as a DC quantity, the survivor
                //    says nothing of itself: "1.2Ω DCR 600" and "1.2Ω DCR
                //    1206" look the same, and one is a package code. A bare
                //    four-digit number ("1206", "2512") may be one too, so
                //    it is no reading on its own.
                [] => {
                    if struck_dc {
                        return None;
                    }
                    if candidates.first().is_some_and(|(tok, _)| is_four_digit_number(&tok.base)) {
                        return None;
                    }
                    if candidates.is_empty() && split.first().is_some_and(|(tok, _)| is_package_code(tok)) {
                        return None;
                    }
                    let start = candidates.first().map_or(0, |(tok, _)| tok.index);
                    let rest: Vec<&str> = split[start..].iter().map(|(tok, _)| tok.as_str()).collect();
                    let norm = normalize_simple_value(&rest.join(" "), "Ω", 'R')?;
                    return BEAD_OHMS.contains(&norm.magnitude).then_some(norm);
                }
                // 3. Two in-band resistances, both marked as such.
                _ => return None,
            }
        }
    };

    let canonical = format_canonical(magnitude, "Ω");
    Some(ValueNorm {
        magnitude,
        unit: "Ω".to_string(),
        canonical: match &freq {
            Some(freq) => format!("{canonical}@{freq}"),
            None => canonical,
        },
        at_frequency: freq,
    })
}

/// The impedance a token could be, if it could be one at all: a value in
/// [`BEAD_OHMS`]. This is the predicate [`normalize_bead_value`] picks
/// candidates with, and [`attach_mark`] binds with, so that a frequency lands
/// on the token the reading will actually choose.
fn bead_ohms(tok: &str) -> Option<f64> {
    if is_package_code(tok) {
        return None;
    }
    parse_value_token(tok, 'R').filter(|magnitude| BEAD_OHMS.contains(magnitude))
}

/// True if `tok` is a bare number with a leading zero ("0402", "0603",
/// "0805"): a package code, never a value. Nobody writes 603Ω as "0603".
/// A decimal ("0.5") is a value.
fn is_package_code(tok: &str) -> bool {
    is_bare_number(tok) && tok.starts_with('0') && tok[1..].starts_with(|c: char| c.is_ascii_digit())
}

/// True if `tok` is four digits and nothing else ("1206", "2512", "1000"):
/// it may be a package code, so a reading that rests on it alone is `null`.
/// Unlike [`is_package_code`] it still counts as a candidate, so it still
/// makes a string ambiguous ("1kΩ 1206 @100MHz" stays `null`).
fn is_four_digit_number(tok: &str) -> bool {
    tok.len() == 4 && tok.chars().all(|c| c.is_ascii_digit())
}

/// One whitespace token of a bead's value string, as [`bead_tokens`] read it.
struct BeadToken {
    /// The token itself; `@` marks are separate tokens by this point, and a
    /// unit split off its number is joined back on ([`bead_split`]).
    base: String,
    /// The frequency the value was quoted at, canonicalized.
    freq: Option<String>,
    /// Whether an "@DC" marked this value as a DC quantity — a DC
    /// resistance, whatever its magnitude — and so not the bead's impedance.
    dc: bool,
    /// Which whitespace token this was, so a reading that falls back to the
    /// resistor parser can start it there rather than at the string's first
    /// token, which may be a rating.
    index: usize,
}

/// What an `@` mark says about the value it follows.
enum Mark {
    /// A frequency: the value is an impedance rating at that point.
    Frequency(String),
    /// The value was measured at DC, so whatever it is, it is not an
    /// impedance rating at a frequency.
    Dc,
    /// Something else. It says nothing either way, so the value it marks
    /// stays a plain unmarked candidate rather than acquiring a frequency
    /// that is not one.
    Other,
}

impl Mark {
    fn read(mark: &str) -> Mark {
        let mark = trim_frequency(mark);
        match frequency_hz(mark) {
            Some(hz) => Mark::from_hz(hz),
            None if is_dc_mark(mark) => Mark::Dc,
            None => Mark::Other,
        }
    }

    /// A frequency in one spelling, so two spellings of one point ("100MHz",
    /// "0.1GHz", "100.0MHz") give one `at_frequency`. Zero hertz is DC
    /// written another way ("@0Hz"), not an impedance rating.
    fn from_hz(hz: f64) -> Mark {
        if hz == 0.0 {
            Mark::Dc
        } else {
            Mark::Frequency(format_canonical(hz, "Hz"))
        }
    }
}

/// Which token a mark binds to.
enum Binding {
    /// An `@` written stuck to a value it can describe is about that value:
    /// "@DC" on any quantity ("200mA@DC"), a frequency on an in-band ohm
    /// value — the same test a spaced one binds with, so "1kΩ 120mΩ@100MHz"
    /// reads like "1kΩ 120mΩ @100MHz". Stuck to anything else, it binds as
    /// [`Binding::Spaced`] does. `at` is the `@`'s own index, to check the
    /// token before it really is the one it was stuck to.
    Glued { at: usize },
    /// An `@` with a space before it is about the nearest value before it
    /// that it can describe: for "@DC", any quantity ("1.2Ω 2kΩ 200mA
    /// @DC"); for a frequency, an in-band ohm value not marked or labelled DC
    /// ("1kΩ 1.2Ω DCR 1.5A @100MHz"). Either steps over a tolerance ("600Ω ±25%
    /// @100MHz") or a word ("1.2Ω DCR @DC").
    Spaced,
    /// A bare frequency literal, with no `@` to say what it belongs to, only
    /// marks a value that says of itself that it is ohms. Adjacency is
    /// weaker evidence, and "1kΩ 0603 100MHz" must not read the package code
    /// as a frequency-marked impedance — that would beat the real value.
    OhmMarked,
}

/// Split a bead's value on whitespace, with every `@` its own token. Each
/// token carries whether an `@` was stuck to the token before it
/// ("1kΩ@100MHz", "1kΩ@ 100MHz") rather than spaced off it ("1kΩ @100MHz"),
/// which decides its [`Binding`]. The flag is always false for other tokens.
///
/// A unit split off its number is joined back on here ("1 kΩ", "1 k ohm",
/// "600 ohm"), before anything binds or counts: otherwise every reading
/// sees a bare "1" and a "kΩ" with no number, and "1.2Ω 1 kΩ 100MHz" reads
/// the DCR as the impedance.
fn bead_split(value: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for raw in value.split_whitespace() {
        let mut after_value = false;
        for (k, part) in raw.split('@').enumerate() {
            if k > 0 {
                out.push(("@".to_string(), after_value));
            }
            after_value = !part.is_empty();
            if !after_value {
                continue;
            }
            match out.last_mut() {
                Some((prev, _)) if k == 0 && joins_split_unit(prev, part) => prev.push_str(part),
                _ => out.push((part.to_string(), false)),
            }
        }
    }
    out
}

/// True if `unit` is the rest of the value `prev` starts. A bare number takes
/// a unit word: an ohm unit ("1 kΩ", "600 ohm", "120 milliohm"), a frequency
/// unit ("100 MHz"), or a current or voltage ("500 mA" is 500mA, a current,
/// not a bare 500 that could be ohms). It does not take any other word: the 600 of
/// "600 bead" or "600 Z @100MHz" is still the impedance, and the 600 of
/// "600 to 1k" is still a rival to the 1k. A number that already took part
/// of an ohm unit ("1k" before "ohm") takes more only while the whole is
/// still an ohm unit, so the K of "4.7 ohm K" stays a tolerance code.
fn joins_split_unit(prev: &str, unit: &str) -> bool {
    let number_len = prev.len() - prev.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.').len();
    if number_len == 0 {
        return false;
    }
    // A unit word starts with a letter: the "100MHz" after "0603" is a
    // frequency of its own, not the package code's unit.
    let word = unit.starts_with(char::is_alphabetic);
    if number_len == prev.len()
        && word
        && (is_frequency_unit(unit) || is_current_or_voltage_unit(unit) || is_ohm_word(unit))
    {
        return true;
    }
    let joined = format!("{}{unit}", &prev[number_len..]);
    strip_long_unit_word(&joined, 'R').is_empty() || parse_unit_only_token(&joined, 'R', true).is_some()
}

/// True if `tok` is an amp or volt unit, in any case, with or without an SI
/// prefix: "A", "mA", "ma", "µA", "Amps", "V", "kV", "VDC", "Vrms",
/// "mAdc". The prefixes are the ones ratings use, lowercase but for kilo
/// ("KV") and mega: "NA" and "PA" are words, not nanoamps and picoamps.
fn is_current_or_voltage_unit(tok: &str) -> bool {
    let is_unit = |word: &str| {
        let lower = word.to_lowercase();
        // "A"/"V" with an optional rms, dc or ac suffix: "Arms", "VDC".
        let letter_unit = lower
            .strip_prefix('a')
            .or_else(|| lower.strip_prefix('v'))
            .is_some_and(|suffix| matches!(suffix, "" | "rms" | "dc" | "ac"));
        letter_unit || matches!(lower.as_str(), "amp" | "amps" | "volt" | "volts")
    };
    let mut chars = tok.chars();
    let prefixed = chars.next().is_some_and(|c| matches!(c, 'p' | 'n' | 'u' | 'µ' | 'μ' | 'm' | 'k' | 'K' | 'M'))
        && is_unit(chars.as_str());
    prefixed || is_unit(tok)
}

/// True if `tok` ends in an ohm word, any case: "ohm", "ohms", or one with
/// the prefix spelled out ("milliohm", "kilohms"). A spelled-out prefix
/// makes the joined value unparseable, which is the point: a DCR written
/// that way is no impedance candidate.
fn is_ohm_word(tok: &str) -> bool {
    strip_long_unit_word(tok, 'R').len() != tok.len()
}

/// True if `tok` is a quantity an `@` can be about: it has a number in it
/// and is not a tolerance ("±25%", "(25%)").
fn is_quantity(tok: &str) -> bool {
    tok.chars().any(|c| c.is_ascii_digit()) && !tok.contains('%')
}

/// True if `tok` is digits and decimal points only: a number with no unit.
fn is_bare_number(tok: &str) -> bool {
    !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Split a bead's value into (value token, frequency) pairs, tolerating the
/// spellings of the `@` that real value fields use: glued to the value
/// ("1kΩ@100MHz"), standing alone ("1kΩ @ 100MHz"), or leaning either way —
/// [`bead_split`] has already split those out. A frequency split across two
/// tokens ("@ 100 MHz") is joined back up, and a bare frequency literal
/// marks the value before it even with no `@` at all ("1kΩ 100MHz").
/// Otherwise the same part normalizes two ways depending only on how it was
/// punctuated, which is the grouping that `at_frequency` exists to make
/// reliable.
///
/// Takes the value already split on whitespace, since the caller needs the
/// tokens too. `None` is the [`attach_mark`] verdict: one value quoted at two
/// different frequencies is a spec table, not a rating.
fn bead_tokens(tokens: &[(String, bool)]) -> Option<Vec<BeadToken>> {
    let mut out: Vec<BeadToken> = Vec::new();
    let mut i = 0;
    // The index of a value a "DCR" label before it struck as DC.
    let mut strike_next: Option<usize> = None;

    while i < tokens.len() {
        let index = i;
        let (tok, glued) = (tokens[i].0.as_str(), tokens[i].1);
        i += 1;

        if tok == "@" {
            let Some((mark, _)) = tokens.get(i) else { continue };
            i += 1;
            let mut mark = mark.clone();
            // "@ 100 MHz": the unit spilled into the token after the number.
            let bare_number = is_bare_number(&mark);
            if let Some((unit, _)) = tokens.get(i).filter(|(t, _)| bare_number && is_frequency_unit(t)) {
                mark.push_str(trim_frequency(unit));
                i += 1;
            }
            let binding = if glued { Binding::Glued { at: index } } else { Binding::Spaced };
            attach_mark(&mut out, Mark::read(&mark), binding)?;
            continue;
        }

        // A frequency literal standing on its own ("100MHz") marks what came
        // before it, the same as an "@" would. A bare "0Hz" is dropped: the
        // ohm-marked binding would walk past an out-of-band DCR and strike
        // out the impedance before it.
        if let Some(hz) = frequency_literal(tok) {
            if hz != 0.0 {
                attach_mark(&mut out, Mark::from_hz(hz), Binding::OhmMarked)?;
            }
            continue;
        }

        // A "DCR" label next to an ohm value says that value is the DC
        // resistance, the same as an "@DC" stuck to it: it is no impedance
        // candidate, and a frequency steps over it. The label binds to the
        // ohm value beside it, before ("1.2Ω DCR 1kΩ") or after ("600R DCR
        // 0.3R"). With one on each side, the smaller is the DCR: a bead's
        // DC resistance is far below its impedance. Only the bare word
        // labels: "DCR=0.3Ω" or "DCR/2A" carries its own value, and a lone
        // "DC" ("600Ω DC") is not a resistance label.
        if tok.eq_ignore_ascii_case("dcr") {
            let before = out
                .last()
                .filter(|prev| prev.index + 1 == index)
                .and_then(|prev| parse_value_token(&prev.base, 'R'));
            let after = tokens.get(index + 1).and_then(|(next, _)| parse_value_token(next, 'R'));
            match (before, after) {
                (Some(b), Some(a)) if a < b => strike_next = Some(index + 1),
                (Some(_), _) => out.last_mut().expect("before is Some").dc = true,
                (None, Some(_)) => strike_next = Some(index + 1),
                (None, None) => {}
            }
        }

        let dc = strike_next == Some(index);
        out.push(BeadToken { base: tok.to_string(), freq: None, dc, index });
    }
    Some(out)
}

/// Apply a mark to the most recent token it can bind to — the `@` is talking
/// about the number before it, and "600 ohm @ 100 MHz" has a word in
/// between. A mark with no value before it at all ("100 MHz 600R") has
/// nothing to say and is dropped.
///
/// `None` means the string quotes one value at two different frequencies,
/// which makes it a spec table rather than a rating: the same verdict
/// [`normalize_bead_value`] reaches for two disagreeing impedances, and for
/// the same reason — taking one by position would make the same part read
/// differently depending on the order someone wrote it in. It also means a
/// frequency landed on a bare number while a value with an ohm unit stands
/// before it ("1kΩ 1206 @100MHz"): either could be the impedance.
fn attach_mark(out: &mut Vec<BeadToken>, mark: Mark, binding: Binding) -> Option<()> {
    // A mark that is neither a frequency nor DC ("@25C") says nothing about
    // the value, wherever it would land.
    if matches!(mark, Mark::Other) {
        return Some(());
    }
    // Where a mark lands when nothing is stuck to it. A DC mark is about the
    // nearest quantity, whatever its unit: "1.2Ω 2kΩ 200mA @DC" is about the
    // 200mA. A frequency is about the nearest value that could be the
    // impedance, stepping over a current ("1kΩ 1.5A @100MHz"), a DCR under
    // the band ("1kΩ 120mΩ 2A @100MHz"), and a value marked or labelled DC
    // ("1kΩ 1.2Ω DCR @100MHz"): each rates the 1kΩ. A leading-zero package
    // code ("0603") is no candidate, so it is stepped over too. Any other
    // bare number is in band, so it is not stepped over ("1kΩ 1206
    // @100MHz" lands on the 1206) — unless the mark already stepped over a
    // quantity to reach it and an ohm-marked value stands before it:
    // "220Ω 1206 2A 50mΩ @100MHz" rates the 220Ω, since the mark plainly
    // skipped the ratings written after the impedance. Only digits count
    // as a bare number: the 2.2k of "1kΩ 2.2k 1.5A @100MHz" could be the
    // impedance.
    let nearest = |out: &[BeadToken]| match mark {
        Mark::Dc => out.iter().rposition(|tok| is_quantity(&tok.base)),
        Mark::Other => None,
        Mark::Frequency(_) => {
            let mut stepped = false;
            (0..out.len()).rev().find(|&i| {
                let tok = &out[i];
                if bead_ohms(&tok.base).is_none() || tok.dc {
                    stepped |= is_quantity(&tok.base);
                    return false;
                }
                // A value struck as DC is no impedance, so it does not count.
                let bare_after_ohms = is_bare_number(&tok.base)
                    && out[..i]
                        .iter()
                        .any(|t| !t.dc && is_ohm_marked_token(&t.base) && bead_ohms(&t.base).is_some());
                !(stepped && bare_after_ohms)
            })
        }
    };
    // What a mark glued to a token is about without looking further: for
    // "@DC", any quantity; for a frequency, an in-band ohm value, as
    // `nearest` uses. A glued frequency on an out-of-band DCR then steps
    // over it as a spaced one does.
    let describes = |tok: &BeadToken| match mark {
        Mark::Dc => is_quantity(&tok.base),
        Mark::Other => false,
        Mark::Frequency(_) => bead_ohms(&tok.base).is_some(),
    };
    // A frequency after a current ("2A@1MHz", "2A @1MHz") rates the ohms
    // before it, unless they already carry a frequency: then it is the
    // current's own rating point ("600Ω@100MHz 2A@1MHz"), not a second
    // impedance. Glued or spaced, the reading is the same.
    let past_quantity = |out: &[BeadToken]| nearest(out).filter(|&p| out[p].freq.is_none());
    let spaced = |out: &[BeadToken]| match out.last() {
        Some(tok) if !describes(tok) && is_quantity(&tok.base) => past_quantity(out),
        _ => nearest(out),
    };
    // A frequency that has to look for its value, with two in-band ohm
    // values before it ("1kΩ 1.5Ω @100MHz"): the nearest is the DCR as often
    // as the impedance, so `null`, as for a bare frequency below. A mark
    // glued straight onto one of them says which it is.
    let two_ohms = matches!(mark, Mark::Frequency(_))
        && out
            .iter()
            .filter(|tok| !tok.dc && bead_ohms(&tok.base).is_some() && is_ohm_marked_token(&tok.base))
            .count()
            > 1;
    let pos = match binding {
        // Stuck to "200mA", an "@DC" is about the 200mA, so it cannot reach
        // back and strike out the 2kΩ of "1.2Ω 2kΩ 200mA@DC". Stuck to
        // something it cannot describe — a word ("1.2Ω DCR@DC", "1kΩ 2A
        // typ@100MHz"), a tolerance ("600Ω ±25%@100MHz"), or, for a
        // frequency, a current ("1kΩ 1.5A@100MHz") — it binds as a spaced
        // one does, so the spacing alone does not change the reading.
        // A unit split off its number ("1 kΩ@100MHz") is already joined back
        // on by [`bead_split`].
        Binding::Glued { at } => match out.last() {
            Some(tok) if tok.index + 1 == at && describes(tok) => Some(out.len() - 1),
            Some(tok) if tok.index + 1 == at => {
                if two_ohms {
                    return None;
                }
                spaced(out)
            }
            _ => None,
        },
        Binding::Spaced => {
            if two_ohms {
                return None;
            }
            spaced(out)
        }
        // Steps over a value struck as DC, as `nearest` does, so "1kΩ 1.2Ω
        // DCR 100MHz" reads like "1kΩ 1.2Ω DCR @100MHz". With two in-band
        // values before it ("1kΩ 1.5Ω 100MHz", "1.2Ω 600 100MHz"), marked
        // or bare, adjacency is all there is to choose by, and it points at
        // the DCR as often as not: `null`.
        Binding::OhmMarked => {
            let in_band = |tok: &BeadToken| !tok.dc && bead_ohms(&tok.base).is_some();
            let ohms = |tok: &BeadToken| in_band(tok) && is_ohm_marked_token(&tok.base);
            if out.iter().filter(|tok| in_band(tok)).count() > 1 {
                return None;
            }
            out.iter().rposition(ohms)
        }
    };
    let Some(pos) = pos else { return Some(()) };
    match mark {
        // A frequency on a value that is no plausible impedance says nothing
        // about the bead: that value is not a candidate to begin with.
        Mark::Frequency(_) if bead_ohms(&out[pos].base).is_none() => {}
        // A bare number took the frequency, but an in-band value that says
        // it is ohms comes first: a package code or MPN fragment may have
        // caught the mark. Do not guess which is the impedance. The same
        // after a value struck as DC: "1.2Ω DCR 600 @100MHz" and "1.2Ω DCR
        // 1206 @100MHz" look alike, and one is a package code.
        Mark::Frequency(_)
            if !is_ohm_marked_token(&out[pos].base)
                && out[..pos]
                    .iter()
                    .any(|tok| tok.dc || (is_ohm_marked_token(&tok.base) && bead_ohms(&tok.base).is_some())) =>
        {
            return None
        }
        Mark::Frequency(freq) => match &out[pos].freq {
            Some(quoted) if quoted != &freq => return None,
            _ => out[pos].freq = Some(freq),
        },
        // "120mΩ@DC" is a DC resistance saying so. It is not an impedance
        // rating at any frequency, so it is not a candidate for the bead's
        // value at all — not even a rival that makes the string ambiguous.
        // Any ohm value counts, in band or not ("120mΩ@DC").
        Mark::Dc if parse_value_token(&out[pos].base, 'R').is_some() => out[pos].dc = true,
        Mark::Dc | Mark::Other => {}
    }
    Some(())
}

/// True if an `@` mark says the value was measured at DC: "DC", "DCR", or
/// either with a separated tail ("DC/2A", like "100MHz/1.5A").
fn is_dc_mark(mark: &str) -> bool {
    let split = mark.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(mark.len());
    let (word, tail) = mark.split_at(split);
    matches!(word.to_ascii_lowercase().as_str(), "dc" | "dcr")
        && !tail.starts_with(|c: char| c.is_ascii_digit())
}

/// Read the frequency out of an `@` mark, in hertz: "100MHZ", "100mhz",
/// "100M", "0.1GHz" and "100MHz/1.5A" all give 1e8. [`Mark::from_hz`] spells
/// it one way, since two spellings of one point have to give one
/// `at_frequency`, or the grouping it exists for splits a part in two.
///
/// A lowercase "m" here is mega, not milli: nothing is rated in millihertz,
/// and reading it that way would invent a part rather than lose one.
///
/// `None` means the mark is not a frequency at all: "@DC", "@25C", or a bare
/// "@100" with neither unit nor prefix to say what it measures.
fn frequency_hz(mark: &str) -> Option<f64> {
    let number: String = mark.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let value: f64 = number.parse().ok()?;
    let rest = &mark[number.len()..];
    let (multiplier, prefixed, rest) = match rest.chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('k') => (1e3, true, &rest[1..]),
        Some('m') => (1e6, true, &rest[1..]),
        Some('g') => (1e9, true, &rest[1..]),
        _ => (1.0, false, rest),
    };
    let (hertz, tail) = match rest.get(..2) {
        Some(unit) if unit.eq_ignore_ascii_case("hz") => (true, &rest[2..]),
        _ => (false, rest),
    };
    // With neither unit nor prefix there is nothing saying this is a
    // frequency; with either, whatever follows has to be separated from it
    // ("100MHz/1.5A") rather than part of it ("25C", "120mΩ").
    if (!hertz && !prefixed) || tail.starts_with(char::is_alphanumeric) {
        return None;
    }
    Some(value * multiplier)
}

/// The frequency a bare token spells out, if it is one — "100MHz", "1GHz".
/// The hertz has to be there: "100M" standing beside a value is a magnitude,
/// not a frequency.
fn frequency_literal(tok: &str) -> Option<f64> {
    let tok = trim_frequency(tok);
    strip_hertz(tok)?;
    frequency_hz(tok)
}

/// `tok` without a trailing "Hz" in any case, or `None` if it has none.
fn strip_hertz(tok: &str) -> Option<&str> {
    let split = tok.len().checked_sub(2)?;
    tok.get(split..)
        .filter(|unit| unit.eq_ignore_ascii_case("hz"))
        .map(|_| &tok[..split])
}

/// Strip the punctuation a value string wraps a frequency in: the comma in
/// "1kΩ@100MHz, 0.5A" is not part of "100MHz". A decimal point in front of
/// a digit is kept: ".1GHz" is 100MHz, not 1GHz.
fn trim_frequency(tok: &str) -> &str {
    let tok = tok.trim_end_matches(|c: char| !c.is_alphanumeric());
    let trimmed = tok.trim_start_matches(|c: char| !c.is_alphanumeric());
    let cut = tok.len() - trimmed.len();
    if tok[..cut].ends_with('.') && trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        &tok[cut - 1..]
    } else {
        trimmed
    }
}

/// True if `tok` is the unit of a frequency someone wrote as two tokens: the
/// "MHz" of "@ 100 MHz", or a lone prefix ("@ 100 M"), which reads the same
/// way it does glued on ("@100M"). Read by [`frequency_hz`] itself, so the
/// two cannot disagree on which prefixes exist.
fn is_frequency_unit(tok: &str) -> bool {
    frequency_hz(&format!("1{}", trim_frequency(tok))).is_some()
}

/// True if `tok` is a parseable ohm value that *says* it is ohms — "120mΩ",
/// "600R", "120ohm" — as opposed to a bare number. That distinction is what
/// lets a lone value be read where it stands: "600R" carries its own unit, so
/// it is the value even when it is not the first token, while a bare "100"
/// may still be waiting for the "k" in the token after it.
fn is_ohm_marked_token(tok: &str) -> bool {
    let stripped = strip_long_unit_word(tok, 'R');
    let marked = stripped.len() != tok.len() || stripped.chars().any(|c| is_unit_char(c, 'R'));
    marked && parse_value_token(tok, 'R').is_some()
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

#[cfg(test)]
mod subsystem_filter_tests {
    use super::SubsystemFilter;

    #[test]
    fn root_selects_only_the_root_sheet() {
        let f = SubsystemFilter::parse(Some("/")).expect("a filter");
        assert_eq!(f, SubsystemFilter::Root);
        assert!(f.matches(Some("/")));
        assert!(!f.matches(Some("/Power/")));
        assert!(!f.matches(Some("/Power/Aux/")));
    }

    #[test]
    fn rooted_path_takes_the_sheet_and_its_descendants() {
        let f = SubsystemFilter::parse(Some("/Power/")).expect("a filter");
        assert!(f.matches(Some("/Power/")));
        assert!(f.matches(Some("/power/aux/")));
        assert!(!f.matches(Some("/")));
        assert!(!f.matches(Some("/PowerMon/")));
        assert!(!f.matches(Some("/Aux/Power/")));
    }

    #[test]
    fn rooted_path_without_trailing_slash_is_the_same_sheet() {
        assert_eq!(
            SubsystemFilter::parse(Some("/Power")),
            SubsystemFilter::parse(Some("/Power/")),
        );
    }

    #[test]
    fn bare_name_stays_a_substring_match() {
        let f = SubsystemFilter::parse(Some("sensor")).expect("a filter");
        assert!(f.matches(Some("/Sensor1/")));
        assert!(f.matches(Some("/Sensor3/")));
        assert!(f.matches(Some("/Analog/Sensor2/")));
        assert!(!f.matches(Some("/Power/")));
        // A bare name never reaches the root sheet — "/" trims to nothing.
        assert!(!f.matches(Some("/")));
    }

    #[test]
    fn blank_argument_is_no_filter() {
        assert_eq!(SubsystemFilter::parse(None), None);
        assert_eq!(SubsystemFilter::parse(Some("")), None);
        assert_eq!(SubsystemFilter::parse(Some("   ")), None);
    }

    #[test]
    fn unmatched_rooted_path_widens_to_a_rooted_prefix() {
        let sheets = [Some("/ADC1/"), Some("/ADC2/"), Some("/Power/")];
        let f = SubsystemFilter::parse(Some("/ADC"))
            .expect("a filter")
            .widen_if_unmatched(sheets.into_iter());
        assert_eq!(f, SubsystemFilter::PathPrefix("/adc".to_string()));
        assert!(f.matches(Some("/ADC1/")));
        assert!(f.matches(Some("/ADC2/")));
        assert!(!f.matches(Some("/Power/")));
    }

    #[test]
    fn widening_stays_anchored_at_the_root() {
        // The case a substring fallback gets wrong: "/E" names no sheet, and
        // as an unanchored substring "e" it selects every sheet here — a
        // rooted argument answering with the whole design, which is #17.
        let sheets = [Some("/Power/"), Some("/Ethernet/"), Some("/Sensor1/")];
        let f = SubsystemFilter::parse(Some("/E"))
            .expect("a filter")
            .widen_if_unmatched(sheets.into_iter());
        assert_eq!(f, SubsystemFilter::PathPrefix("/e".to_string()));
        assert!(f.matches(Some("/Ethernet/")));
        assert!(!f.matches(Some("/Power/")));
        assert!(!f.matches(Some("/Sensor1/")));
        // Nor does a prefix reach a sheet that merely contains the name
        // deeper down, the way a substring would.
        assert!(!f.matches(Some("/Analog/Ethernet/")));
        // The unanchored reading is still one keystroke away.
        let bare = SubsystemFilter::parse(Some("e")).expect("a filter");
        assert!(bare.matches(Some("/Power/")));
    }

    #[test]
    fn widening_does_not_reach_a_differently_rooted_sheet() {
        // "/USB" in a design whose only USB sheet is /PeriphUSB/: a substring
        // fallback would hand back that sheet's parts as if they were the
        // ones asked for.
        let sheets = [Some("/PeriphUSB/"), Some("/Power/")];
        let f = SubsystemFilter::parse(Some("/USB"))
            .expect("a filter")
            .widen_if_unmatched(sheets.into_iter());
        assert_eq!(f, SubsystemFilter::PathPrefix("/usb".to_string()));
        assert!(!f.matches(Some("/PeriphUSB/")));
    }

    #[test]
    fn rooted_path_that_matches_a_sheet_stays_a_path() {
        let sheets = [Some("/Power/"), Some("/PowerMon/")];
        let f = SubsystemFilter::parse(Some("/Power"))
            .expect("a filter")
            .widen_if_unmatched(sheets.into_iter());
        assert_eq!(f, SubsystemFilter::Path("/power/".to_string()));
        // Still a path, so the sibling sheet is still excluded — widening it
        // would have swept /PowerMon/ in.
        assert!(!f.matches(Some("/PowerMon/")));
    }

    #[test]
    fn root_never_widens() {
        // A design with no root-sheet parts: the empty answer is the true one.
        let sheets = [Some("/Power/"), Some("/ADC1/")];
        let f = SubsystemFilter::parse(Some("/"))
            .expect("a filter")
            .widen_if_unmatched(sheets.into_iter());
        assert_eq!(f, SubsystemFilter::Root);
    }

    #[test]
    fn unassigned_sheet_matches_nothing() {
        for arg in ["/", "/Power/", "power"] {
            let f = SubsystemFilter::parse(Some(arg)).expect("a filter");
            assert!(!f.matches(None), "{arg} should not match an unassigned part");
            assert!(!f.matches(Some("")), "{arg} should not match an empty sheet");
        }
        let widened = SubsystemFilter::PathPrefix("/power".to_string());
        assert!(!widened.matches(None));
        assert!(!widened.matches(Some("")));
    }
}

#[cfg(test)]
mod subsystem_filter_seam_tests {
    use super::{CompId, Component, Design, SubsystemFilter};
    use std::collections::HashMap;

    /// A design whose components carry nothing but a sheet, which is all
    /// `Design::subsystem_filter` reads off them.
    fn design_with_sheets(sheets: &[&str]) -> Design {
        let components = sheets
            .iter()
            .enumerate()
            .map(|(i, sheet)| Component {
                id: CompId(i),
                refdes: format!("U{i}"),
                value: String::new(),
                value_norm: None,
                footprint: None,
                description: None,
                sheet: Some(sheet.to_string()),
                dnp: false,
                exclude_from_bom: false,
                properties: HashMap::new(),
                pins: Vec::new(),
            })
            .collect();
        let sheet_paths = super::sheet_path_set(sheets.iter().copied());
        Design {
            components,
            pins: Vec::new(),
            nets: Vec::new(),
            component_map: HashMap::new(),
            pin_map: HashMap::new(),
            net_map: HashMap::new(),
            sheet_paths,
        }
    }

    #[test]
    fn absent_argument_yields_no_filter_and_no_note() {
        let design = design_with_sheets(&["/Power/"]);
        let (filter, note) = design.subsystem_filter(None);
        assert!(filter.is_none());
        assert!(note.is_none());
    }

    #[test]
    fn a_path_the_design_answers_to_is_taken_at_face_value() {
        let design = design_with_sheets(&["/Power/", "/PowerMon/"]);
        let (filter, note) = design.subsystem_filter(Some("/Power"));
        assert_eq!(filter, Some(SubsystemFilter::Path("/power/".to_string())));
        // Nothing was reinterpreted, so the envelope stays quiet.
        assert!(note.is_none());
    }

    #[test]
    fn an_unmatched_path_widens_and_says_so() {
        let design = design_with_sheets(&["/ADC1/", "/ADC2/", "/Power/"]);
        let (filter, note) = design.subsystem_filter(Some("/ADC"));
        assert_eq!(filter, Some(SubsystemFilter::PathPrefix("/adc".to_string())));
        let note = note.expect("a widened filter must be reported");
        assert!(note.contains("/ADC"), "{note}");
        assert!(note.contains("starts with '/adc'"), "{note}");
    }

    #[test]
    fn root_and_bare_names_never_produce_a_note() {
        let design = design_with_sheets(&["/Power/", "/Sensor1/"]);
        for arg in ["/", "sensor", "nosuchsheet"] {
            let (filter, note) = design.subsystem_filter(Some(arg));
            assert!(filter.is_some(), "{arg}");
            // Only a rooted path is ever reinterpreted; a bare name that
            // matches nothing is a true empty answer, same as Root.
            assert!(note.is_none(), "{arg} produced {note:?}");
        }
    }

    /// The note is only useful if it survives serialization — it is the one
    /// thing on the page that says the argument was reinterpreted.
    #[test]
    fn a_widened_filter_reports_itself_in_the_envelope() {
        let design = design_with_sheets(&["/ADC1/", "/ADC2/", "/Power/"]);
        let json = design
            .filter_components(None, None, Some("/ADC"), None, 10, 0)
            .expect("filter_components");
        assert!(json.contains("subsystem_note"), "{json}");
        assert!(json.contains("drop the leading slash"), "{json}");
        // The widened prefix matched, but the note does not say so either
        // way — an empty page carries the same wording.
        assert!(!json.contains("matched sheets"), "{json}");

        // Taken at face value, the envelope keeps its compact shape.
        let json = design
            .filter_components(None, None, Some("/Power"), None, 10, 0)
            .expect("filter_components");
        assert!(!json.contains("subsystem_note"), "{json}");
    }
}

#[cfg(test)]
mod subsystem_display_name_tests {
    use super::subsystem_display_name;

    #[test]
    fn trims_the_rooting_slashes() {
        assert_eq!(subsystem_display_name("/Power/"), "Power");
        assert_eq!(subsystem_display_name("/Power/Aux/"), "Power/Aux");
    }

    #[test]
    fn root_keeps_its_slash_so_it_round_trips_as_a_selector() {
        use super::SubsystemFilter;
        let name = subsystem_display_name("/");
        assert_eq!(name, "/");
        // The name an agent reads back out must select the root sheet, not
        // the whole design.
        assert_eq!(SubsystemFilter::parse(Some(&name)), Some(SubsystemFilter::Root));
    }
}

#[cfg(test)]
mod value_norm_tests {
    use super::{normalize_value, presence_flag};
    use std::collections::HashMap;

    #[test]
    fn parses_pf_written_as_trailing_token() {
        let v = normalize_value("33 pF", "C").expect("should parse");
        assert_eq!(v.canonical, "33pF");
        assert!((v.magnitude - 33e-12).abs() < 1e-20);
    }

    #[test]
    fn parses_pf_written_as_prefix_suffix() {
        let v = normalize_value("33p", "C").expect("should parse");
        assert_eq!(v.canonical, "33pF");
        assert!((v.magnitude - 33e-12).abs() < 1e-20);
    }

    #[test]
    fn parses_kilohm_resistor() {
        let v = normalize_value("348k", "R").expect("should parse");
        assert_eq!(v.canonical, "348kΩ");
        assert!((v.magnitude - 348000.0).abs() < 1e-9);
    }

    #[test]
    fn parses_microfarad_with_voltage_rating() {
        let v = normalize_value("4.7u 25V", "C").expect("should parse");
        assert_eq!(v.canonical, "4.7µF");
        assert!((v.magnitude - 4.7e-6).abs() < 1e-15);
    }

    #[test]
    fn zero_ohm_resistor() {
        let v = normalize_value("0", "R").expect("should parse");
        assert_eq!(v.canonical, "0Ω");
        assert_eq!(v.magnitude, 0.0);
    }

    #[test]
    fn decimal_marker_style_resistor() {
        let v = normalize_value("4k7", "R").expect("should parse");
        assert!((v.magnitude - 4700.0).abs() < 1e-9);

        let v = normalize_value("4R7", "R").expect("should parse");
        assert!((v.magnitude - 4.7).abs() < 1e-9);
    }

    #[test]
    fn trailing_unit_letter_on_resistor() {
        let v = normalize_value("10R", "R").expect("should parse");
        assert!((v.magnitude - 10.0).abs() < 1e-9);
    }

    #[test]
    fn prefix_and_unit_letter_concatenated() {
        let v = normalize_value("0.1uF", "C").expect("should parse");
        assert!((v.magnitude - 1e-7).abs() < 1e-15);
    }

    #[test]
    fn uppercase_prefix_folds_where_it_is_unambiguous() {
        // Issue #18: "24K" is the same 24k every other resistor spells
        // lowercase, and a null here reads downstream as missing data rather
        // than as a formatting variant.
        let v = normalize_value("24K", "R").expect("should parse");
        assert_eq!(v.canonical, "24kΩ");
        assert!((v.magnitude - 24000.0).abs() < 1e-9);

        for (raw, class, canonical) in [
            ("33P", "C", "33pF"),
            ("10N", "C", "10nF"),
            ("4U7", "C", "4.7µF"),
            ("1K5", "R", "1.5kΩ"),
            ("2U2", "L", "2.2µH"),
        ] {
            let v = normalize_value(raw, class).unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, canonical, "{raw}");
        }
    }

    #[test]
    fn milli_and_mega_keep_their_case_on_a_resistor() {
        // The one place folding must not touch: both cases are real for a
        // resistor, and case is all that separates a shunt from a bleeder.
        let milli = normalize_value("1m", "R").expect("should parse");
        assert_eq!(milli.canonical, "1mΩ");
        let mega = normalize_value("1M", "R").expect("should parse");
        assert_eq!(mega.canonical, "1MΩ");
    }

    #[test]
    fn uppercase_m_is_milli_on_an_inductor() {
        // No inductor is a megahenry, so an uppercase 'M' there can only be
        // the millihenry — the same no-other-quantity argument that folds
        // 'K' and 'P'. Case-converted BOMs are where this spelling comes from.
        for (raw, canonical) in [("10MH", "10mH"), ("1MH", "1mH"), ("1M5", "1.5mH")] {
            let v = normalize_value(raw, "L").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, canonical, "{raw}");
        }
        // Lowercase is unaffected, and neither reading of 'M' is safe on a
        // capacitor — the megafarad is not a part, and legacy "MF"/"MFD"
        // spells the microfarad — so C keeps its null.
        let v = normalize_value("10mH", "L").expect("should parse");
        assert_eq!(v.canonical, "10mH");
        assert!(normalize_value("1MF", "C").is_none());
        assert!(normalize_value("475M", "C").is_none());
    }

    #[test]
    fn a_lone_prefix_token_supplies_the_multiplier() {
        // Before #18 these fell through to a multiplier of 1 — "100 k" came
        // back as 100Ω, a wrong number rather than a missing one.
        let v = normalize_value("100 k", "R").expect("should parse");
        assert_eq!(v.canonical, "100kΩ");

        let v = normalize_value("22 u 25 V", "C").expect("should parse");
        assert_eq!(v.canonical, "22µF");

        let v = normalize_value("24 Kohm", "R").expect("should parse");
        assert_eq!(v.canonical, "24kΩ");
    }

    #[test]
    fn a_bare_unit_word_is_still_no_multiplier() {
        // "ohms" strips to nothing, which must stay a multiplier of 1 rather
        // than becoming a prefix lookup on some leftover character.
        let v = normalize_value("24 ohms", "R").expect("should parse");
        assert_eq!(v.canonical, "24Ω");
    }

    #[test]
    fn both_micro_signs_parse() {
        // U+00B5 MICRO SIGN and U+03BC GREEK SMALL LETTER MU, which exports
        // use interchangeably.
        for raw in ["4.7\u{b5}F", "4.7\u{3bc}F"] {
            let v = normalize_value(raw, "C").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "4.7µF");
        }
    }

    #[test]
    fn a_part_number_in_the_value_field_stays_unparsed() {
        // Folding uppercase prefixes puts 'P'/'N'/'K'/'U' in reach of MPNs
        // that some designs put in a passive's value. The trailing junk after
        // the marker is what keeps them out: 4816P must not read as 4816p.
        assert!(normalize_value("4816P-1-103LF", "R").is_none());
        assert!(normalize_value("GRM155R71C104KA88D", "C").is_none());

        // Semiconductor part numbers are all digits around the folded letter,
        // so only the length of the digit run separates "2N7002" from "4k7".
        for raw in ["2N7002", "1N4148", "2N3904", "1N5819"] {
            assert!(normalize_value(raw, "R").is_none(), "{raw}");
        }
        // Four significant figures is still a value, not a part number.
        let v = normalize_value("1K00", "R").expect("should parse");
        assert_eq!(v.canonical, "1kΩ");
    }

    #[test]
    fn a_prefix_the_class_is_not_made_in_is_refused() {
        // "104K" is 100nF ±10% — the letter is the tolerance, not a prefix,
        // and no capacitor or inductor is measured in kilos.
        for raw in ["104K", "103K", "224K", "104M"] {
            assert!(normalize_value(raw, "C").is_none(), "{raw}");
        }
        assert!(normalize_value("101K", "L").is_none());
        // The same shape on a resistor is a real value and still parses.
        let v = normalize_value("104K", "R").expect("should parse");
        assert_eq!(v.canonical, "104kΩ");

        // The other end of the same rule: no resistor is a nanoohm or a
        // picoohm, so those letters are a misread of something else.
        for raw in ["100n", "4n7", "33P", "1n5"] {
            assert!(normalize_value(raw, "R").is_none(), "{raw}");
        }
        // The floor sits at the microohm shunt, which is a real part.
        for (raw, canonical) in [("100u", "100µΩ"), ("500m", "500mΩ"), ("1G", "1GΩ")] {
            let v = normalize_value(raw, "R").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, canonical, "{raw}");
        }
    }

    #[test]
    fn a_stray_letter_is_not_a_multiplier() {
        // The lone-prefix rule reaches only the token after the number.
        // Further out, a single letter is a tolerance or dielectric code:
        // "4.7 ohm K" is a ±10% 4.7Ω part, not 4.7kΩ.
        let v = normalize_value("4.7 ohm K", "R").expect("should parse");
        assert_eq!(v.canonical, "4.7Ω");

        let v = normalize_value("22 uF X7R K", "C").expect("should parse");
        assert_eq!(v.canonical, "22µF");

        // A split-off letter the class cannot be measured in is no multiplier
        // either, adjacent or not: "100 n" is not a 100nΩ resistor.
        let v = normalize_value("100 n", "R").expect("should parse");
        assert_eq!(v.canonical, "100Ω");
    }

    #[test]
    fn rejects_placeholder_junk() {
        assert!(normalize_value("placeholder", "C").is_none());
    }

    #[test]
    fn rejects_bare_class_letter() {
        assert!(normalize_value("R", "R").is_none());
    }

    #[test]
    fn non_passive_class_returns_none() {
        assert!(normalize_value("STM32F407VGT6", "U").is_none());
    }

    #[test]
    fn a_bead_reads_its_impedance_not_its_dc_resistance() {
        // Issue #20. The compound form real boards use: current rating, DC
        // resistance, impedance@frequency. The first parseable token is the
        // 120mΩ DCR — reporting that would be a confident wrong answer.
        let v = normalize_value("1.5A 120mΩ 1kΩ@100MHz", "FB").expect("should parse");
        assert_eq!(v.canonical, "1kΩ@100MHz");
        assert_eq!(v.unit, "Ω");
        assert_eq!(v.at_frequency.as_deref(), Some("100MHz"));
        assert!((v.magnitude - 1000.0).abs() < 1e-9);

        let v = normalize_value("450mA 290mΩ 220Ω@100MHz", "FB").expect("should parse");
        assert_eq!(v.canonical, "220Ω@100MHz");
        assert!((v.magnitude - 220.0).abs() < 1e-9);

        // The same rule with the current rating left off, and on its own —
        // the bare "@freq" suffix used to defeat the parser entirely.
        for raw in ["120mΩ 1kΩ@100MHz", "1kΩ@100MHz"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
        }
    }

    #[test]
    fn a_single_valued_bead_reads_like_a_resistor() {
        for (raw, canonical) in [("600R", "600Ω"), ("1kΩ", "1kΩ"), ("120 ohm", "120Ω")] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, canonical, "{raw}");
            assert_eq!(v.at_frequency, None, "{raw}");
        }
    }

    #[test]
    fn an_unmarked_pair_of_ohm_values_stays_null() {
        // Two values that could each be the impedance and nothing to tell
        // them apart. Null is the honest answer here; picking either one is a
        // coin flip that reads downstream as fact. 1.5Ω is the high end of a
        // small-signal bead's DCR, so the band cannot rule it out.
        assert!(normalize_value("1.5Ω 1kΩ", "FB").is_none());
        // Two bare in-band numbers, one of which is the "500" of a split
        // current rating. Nothing marks either as the ohms.
        assert!(normalize_value("500 mA 100 k", "FB").is_none());
    }

    #[test]
    fn a_value_behind_a_rating_reads_from_its_own_token() {
        // The resistor reading takes the FIRST token for the value, which
        // here is a rating it cannot parse at all — so a bead whose only
        // in-band ohm value sits further along used to come back null. With
        // the DCR out of the candidate pool by band, that value is alone and
        // unambiguous, exactly as it is in the ohm-marked "2A 600R".
        for (raw, canonical) in
            [("1.5A 120mΩ 1k", "1kΩ"), ("2A 1k", "1kΩ"), ("0.5A 100", "100Ω")]
        {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, canonical, "{raw}");
        }
    }

    #[test]
    fn a_lone_sub_ohm_value_is_a_dc_resistance_not_a_bead() {
        // The impedance is left to the MPN and only the rating and the DCR
        // are written out. With no second ohm value there is nothing to trip
        // the ambiguity rule, so only the plausibility band stands between
        // this and reporting a 0.12Ω "bead" (issue #20).
        assert!(normalize_value("1.5A 120mΩ", "FB").is_none());
        assert!(normalize_value("120mΩ", "FB").is_none());
        // ...and once out of the way, a DCR no longer makes its own string
        // ambiguous: 120mΩ is not a candidate, so the 1kΩ stands alone.
        let v = normalize_value("120mΩ 1kΩ", "FB").expect("should parse");
        assert_eq!(v.canonical, "1kΩ");
    }

    #[test]
    fn a_bare_number_out_of_band_is_a_part_number() {
        // Bead value fields commonly hold the MPN, and a numeric one parses
        // as a perfectly good ohm value: 742792625 is a Würth WE-CBF part,
        // not a 742.793MΩ part. Only the band catches it.
        assert!(normalize_value("742792625", "FB").is_none());
        assert!(normalize_value("BLM18PG121SN1D", "FB").is_none());
    }

    #[test]
    fn a_bead_ignores_ratings_that_are_not_ohms() {
        // Only values that could plausibly be the impedance count towards
        // the ambiguity test — in-band, and not marked as measured at DC —
        // or every compound string would look like it held two impedances.
        // Bare in-band numbers do count: see the "500 mA 100 k" case above.
        let v = normalize_value("1.5A 1kΩ@100MHz 25V", "FB").expect("should parse");
        assert_eq!(v.canonical, "1kΩ@100MHz");
        // ...and with no marked token at all, the resistor reading still
        // applies to the one value present.
        let v = normalize_value("2A 600R", "FB").expect("should parse");
        assert_eq!(v.canonical, "600Ω");
    }

    #[test]
    fn two_disagreeing_impedance_points_stay_null() {
        // A spec table pasted into the value field, not a rating. Taking one
        // by position would make the same part read differently depending on
        // the order it was written in — the opposite of what at_frequency is
        // for.
        assert!(normalize_value("600Ω@10MHz 1kΩ@100MHz", "FB").is_none());
        assert!(normalize_value("1kΩ@100MHz 600Ω@10MHz", "FB").is_none());
        // A DCR quoted at DC is not a second opinion about the impedance.
        // Under the band it never becomes a candidate at all; inside it —
        // 1.5Ω is a real small-signal DCR — the "@DC" is what rules it out,
        // and saying so must not cost the reading.
        for raw in ["1kΩ@100MHz 120mΩ@DC", "1kΩ@100MHz 1.5Ω@DC", "1.5Ω@DC 1kΩ@100MHz"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
        }
        // On its own it is a DC resistance and nothing else.
        assert!(normalize_value("1.5Ω@DC", "FB").is_none());
        // Two points that agree on the ohms but not on where they were
        // measured are still a table: at_frequency is part of the identity,
        // so taking one by position splits the part in two.
        assert!(normalize_value("1kΩ@100MHz 1kΩ@1GHz", "FB").is_none());
        assert!(normalize_value("1kΩ@1GHz 1kΩ@100MHz", "FB").is_none());
        // The same figure quoted twice is not a disagreement.
        let v = normalize_value("1kΩ@100MHz 1kΩ@100MHz", "FB").expect("should parse");
        assert_eq!(v.canonical, "1kΩ@100MHz");
    }

    #[test]
    fn an_at_mark_binds_to_the_quantity_it_follows() {
        // A mark on a current rating is about the current rating. Walking
        // back past it to an ohm value struck out the impedance and read the
        // DCR, or took the rating's frequency as a second impedance point.
        let v = normalize_value("1.2Ω 2kΩ 200mA@DC", "FB");
        assert!(v.is_none(), "{v:?}");
        for raw in ["600Ω@100MHz 1.5A@DC", "600Ω@100MHz 2A@1MHz", "600Ω@100MHz 2A @1MHz"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "600Ω@100MHz", "{raw}");
        }
    }

    #[test]
    fn review_27_round_8_decisions() {
        for (raw, want) in [
            // A frequency that stepped over ratings also steps over a bare
            // package code behind an ohm-marked value...
            ("220Ω 0402 2A 50mΩ @100MHz", Some("220Ω@100MHz")),
            // ...but not one it reached without stepping over anything.
            ("1kΩ 1206 @100MHz", None),
            ("220Ω 1206 2A 50mΩ @100MHz", Some("220Ω@100MHz")),
            // "NA" and "PA" are words, not nanoamps and picoamps.
            ("600 NA", Some("600Ω")),
            ("600 PA", Some("600Ω")),
            ("1k 500 nA", Some("1kΩ")),
            // A and V take an rms, dc or ac suffix, any case.
            ("1k 25 Vrms", Some("1kΩ")),
            ("1k 500 mAdc", Some("1kΩ")),
            ("1k 2 Arms", Some("1kΩ")),
            ("1k 25 vac", Some("1kΩ")),
            // "DCR" labels the value before it as DC, wherever the frequency is.
            ("1.2Ω DCR 1kΩ", Some("1kΩ")),
            ("1kΩ 1.2Ω DCR @100MHz", Some("1kΩ@100MHz")),
            ("1kΩ@100MHz 1.2Ω DCR", Some("1kΩ@100MHz")),
            // A bare frequency steps over a labelled DCR as an "@" one does.
            ("1kΩ 1.2Ω DCR 100MHz", Some("1kΩ@100MHz")),
            ("1kΩ 1.2Ω DCR 2A 100MHz", Some("1kΩ@100MHz")),
            // Only the bare word labels. A label with its own value, or a
            // lone "DC", leaves the value before it alone.
            ("1kΩ DCR=0.3Ω", Some("1kΩ")),
            ("600Ω DCR:0.3Ω", Some("600Ω")),
            ("1kΩ DCR/2A", Some("1kΩ")),
            ("600Ω DC", Some("600Ω")),
            ("600 DC 2A", Some("600Ω")),
            // A struck DCR is no ohm value for a package code to stand behind.
            ("1.2Ω DCR 0603 2A @100MHz", None),
            ("1.2Ω@DC 0603 1A @100MHz", None),
            // Round 9 decisions. A leading-zero bare number is a package
            // code, never a candidate.
            ("1kΩ 0603 @100MHz", Some("1kΩ@100MHz")),
            ("600 0603 @100MHz", Some("600Ω@100MHz")),
            ("0603 12kΩ@100MHz", None),
            ("0.3Ω DCR 0603 2A @100MHz", None),
            ("0603 2A", None),
            // Once a value is struck as DC, a bare survivor does not win.
            ("1.2Ω DCR 0603", None),
            ("1.2Ω@DC 0603", None),
            ("1.2Ω DCR 600", None),
            ("1.2Ω DCR 1kΩ", Some("1kΩ")),
            // ...nor one a frequency names (corpus rows 160-162, 169).
            ("1.2Ω DCR 600 @100MHz", None),
            ("1.2Ω DCR 600@100MHz", None),
            // A DCR label binds to the ohm value beside it, and with one on
            // each side, to the smaller.
            ("600R DCR 0.3R", Some("600Ω")),
            ("600Ω DCR 0.3Ω", Some("600Ω")),
            ("600 ohm DCR 0.3 ohm", Some("600Ω")),
            ("600R DCR 300mR", Some("600Ω")),
            ("1kΩ DCR 1.2Ω", Some("1kΩ")),
            ("1kΩ DCR 1.2Ω @100MHz", Some("1kΩ@100MHz")),
            // With nothing after it, the label is about the value before.
            ("600 ohm DCR @100MHz", None),
        ] {
            let got = normalize_value(raw, "FB").map(|v| v.canonical);
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn dc_reads_as_dc_however_it_is_written() {
        for raw in ["1.5Ω@DC,", "1.5Ω@(DC)", "1.5Ω@DC, 2A", "1.5Ω@DC/2A", "1.5Ω@0Hz", "0.5 kΩ@DC"] {
            let v = normalize_value(raw, "FB");
            assert!(v.is_none(), "{raw}: {v:?}");
        }
        // A bare "0Hz" has no "@" to say what it means, so it strikes
        // nothing, least of all the impedance two tokens back.
        for raw in ["1kΩ@100MHz 1.5Ω@0Hz", "1kΩ@100MHz 120mΩ 0Hz"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
        }
    }

    #[test]
    fn an_at_reads_past_a_tolerance_and_a_split_unit() {
        // A tolerance is no value, and a unit split off its number belongs
        // to it: "1 kΩ@100MHz" is 1kΩ, not 1Ω.
        for (raw, want) in [
            ("600Ω ±25% @100MHz", "600Ω@100MHz"),
            ("220Ω (±25%) @ 100MHz", "220Ω@100MHz"),
            ("1 kΩ@100MHz", "1kΩ@100MHz"),
            ("2.2 k @ 100MHz", "2.2kΩ@100MHz"),
        ] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, want, "{raw}");
        }
    }

    #[test]
    fn only_a_unit_word_joins_a_bare_number() {
        for (raw, want) in [
            ("600 Ferrite Bead", Some("600Ω")),
            ("600 bead", Some("600Ω")),
            ("120 FB", Some("120Ω")),
            ("600 Z @100MHz", Some("600Ω@100MHz")),
            ("600 typ @100MHz", Some("600Ω@100MHz")),
            // Not "600to" and a lone 1k: two rivals, no unit on either.
            ("600 to 1k", None),
            // A current or voltage still joins, so it is not a candidate.
            ("500 mA 1kΩ", Some("1kΩ")),
            ("1kΩ 25 V", Some("1kΩ")),
        ] {
            let got = normalize_value(raw, "FB").map(|v| v.canonical);
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn a_frequency_steps_over_a_current_to_the_ohms() {
        for (raw, want) in [
            ("1kΩ 1.5A @100MHz", Some("1kΩ@100MHz")),
            ("220Ω 0.5A max @ 100MHz", Some("220Ω@100MHz")),
            ("1kΩ 1.5A ±25% @100MHz", Some("1kΩ@100MHz")),
            ("1kΩ (0603) @100MHz", Some("1kΩ@100MHz")),
            ("1kΩ 1.5A, @100MHz", Some("1kΩ@100MHz")),
            // Two in-band ohm values before the mark: `null` (corpus row 82).
            ("1.2Ω 600Ω 2A @100MHz", None),
            ("1kΩ 2A typ@100MHz", Some("1kΩ@100MHz")),
        ] {
            let got = normalize_value(raw, "FB").map(|v| v.canonical);
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn a_rating_word_joins_in_any_case_and_spelling() {
        for (raw, want) in [
            ("1k 2 Amps", Some("1kΩ")),
            ("1k 1.5 Amp", Some("1kΩ")),
            ("1k 500 ma", Some("1kΩ")),
            ("1k 2 a", Some("1kΩ")),
            ("1k 25 v", Some("1kΩ")),
            ("1k 25 VDC", Some("1kΩ")),
            ("1k 25 Vdc", Some("1kΩ")),
            ("1k 25 KV", Some("1kΩ")),
            ("1k 120 milliohm", Some("1kΩ")),
            ("600 1.5 Amp", Some("600Ω")),
        ] {
            let got = normalize_value(raw, "FB").map(|v| v.canonical);
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn a_frequency_skips_what_cannot_be_the_impedance() {
        for (raw, want) in [
            // Glued or spaced, a current is stepped over.
            ("1kΩ 1.5A@100MHz", Some("1kΩ@100MHz")),
            // A DCR under the band, or one a "DCR" labels, is too.
            ("1kΩ 120mΩ 2A @100MHz", Some("1kΩ@100MHz")),
            ("1kΩ 1.2Ω DCR @100MHz", Some("1kΩ@100MHz")),
            // A glued frequency on an ohm value stays on it.
            ("1kΩ@100MHz", Some("1kΩ@100MHz")),
            // A bare in-band number behind an ohm value: either could be
            // the impedance, with or without a current in between.
            ("1kΩ 2.2k @100MHz", None),
            ("1kΩ 2.2k 1.5A @100MHz", None),
        ] {
            let got = normalize_value(raw, "FB").map(|v| v.canonical);
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn a_split_unit_is_one_value_before_anything_binds() {
        // Joined in the tokenizer, so a bare frequency and the candidate
        // count see "1kΩ", not a "1" and a "kΩ".
        for (raw, want) in [
            ("1 k ohm@100MHz", Some("1kΩ@100MHz")),
            ("1 kΩ 100MHz", Some("1kΩ@100MHz")),
            // Two ohm values before a bare frequency: `null` (corpus row 83),
            // but not the 1.2Ω, as an unjoined "1" and "kΩ" would give.
            ("1.2Ω 1 kΩ 100MHz", None),
            ("1.5Ω 1 kΩ", None),
            ("600 ohm @ 100 MHz", Some("600Ω@100MHz")),
            ("1k @ 100 M", Some("1kΩ@100MHz")),
        ] {
            let got = normalize_value(raw, "FB").map(|v| v.canonical);
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn an_at_binds_to_the_nearest_quantity_glued_or_spaced() {
        // Stuck to the 200mA or spaced off it, the "@DC" is about the
        // current and says nothing about the 2kΩ.
        for raw in ["1.2Ω 2kΩ 200mA@DC", "1.2Ω 2kΩ 200mA @DC", "1.2Ω 2kΩ 200mA @ DC"] {
            let v = normalize_value(raw, "FB");
            assert!(v.is_none(), "{raw}: 1.2Ω and 2kΩ stay ambiguous: {v:?}");
        }
        // Nor does a spaced "@DC" after a current reach the rated impedance.
        for (raw, want) in [
            ("1kΩ 1.5Ω @DC", "1kΩ"),
            ("1kΩ@100MHz 1.5A @DC", "1kΩ@100MHz"),
            ("1 kΩ 1.5A @DC", "1kΩ"),
            // Glued to a tolerance, it binds as a spaced one does.
            ("600Ω ±25%@100MHz", "600Ω@100MHz"),
            ("1kΩ 1.2Ω DCR@DC", "1kΩ"),
            // Glued to an out-of-band DCR, a frequency steps over it as a
            // spaced one does.
            ("1kΩ 120mΩ@100MHz", "1kΩ@100MHz"),
            ("1kΩ 120mΩ @100MHz", "1kΩ@100MHz"),
        ] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, want, "{raw}");
        }
    }

    #[test]
    fn a_frequency_on_a_bare_number_after_an_ohm_value_stays_null() {
        // "1206" took the "@", but the 1kΩ before it says it is ohms: either
        // could be the impedance, so do not guess. ("0603" is a package code
        // and never a candidate, so "1kΩ 0603 @100MHz" is the 1kΩ.)
        assert!(normalize_value("1kΩ 1206 @100MHz", "FB").is_none());
        for (raw, want) in [("600@100MHz", "600Ω@100MHz"), ("0603 1kΩ@100MHz", "1kΩ@100MHz")] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, want, "{raw}");
        }
    }

    #[test]
    fn one_frequency_point_has_one_spelling() {
        for raw in ["1kΩ@0.1GHz", "1kΩ@.1GHz", "1kΩ@100.0MHz", "1kΩ@100000kHz", "1k @ 100 M"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
            assert_eq!(v.at_frequency.as_deref(), Some("100MHz"), "{raw}");
        }
        let v = normalize_value("1kΩ@100MHz 1kΩ@0.1GHz", "FB").expect("should parse");
        assert_eq!(v.canonical, "1kΩ@100MHz");
    }

    #[test]
    fn a_frequency_reads_the_same_however_it_is_punctuated() {
        // Same part, six spellings. Any of these normalizing differently
        // would split one bead into several lines of a sweep, which is the
        // whole reason at_frequency exists.
        for raw in [
            "1kΩ@100MHz",
            "1kΩ @100MHz",
            "1kΩ@ 100MHz",
            "1kΩ @ 100MHz",
            "1kΩ @ 100 MHz",
            "1kΩ 100MHz",
        ] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
            assert_eq!(v.at_frequency.as_deref(), Some("100MHz"), "{raw}");
        }
        // The unit spelled out, and the "@" talking past it to the number.
        let v = normalize_value("600 ohm @ 100 MHz", "FB").expect("should parse");
        assert_eq!(v.canonical, "600Ω@100MHz");
        assert_eq!(v.at_frequency.as_deref(), Some("100MHz"));
    }

    #[test]
    fn a_bare_frequency_only_marks_a_value_that_says_it_is_ohms() {
        // With no "@" to say what it belongs to, a frequency binds by
        // adjacency, which is weak evidence — so it only marks a value that
        // carries its own unit, and only one that could be an impedance at
        // all. Otherwise a package code or a DC resistance standing between
        // the value and the frequency would take the mark, and a marked
        // token beats every other rule.
        for raw in ["1kΩ 0603 100MHz", "1kΩ 120mΩ 100MHz", "1kΩ 100MHz 120mΩ"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
        }
        // An "@" says which value it means, so it may mark a bare number.
        let v = normalize_value("600@100MHz", "FB").expect("should parse");
        assert_eq!(v.canonical, "600Ω@100MHz");
        // A lone package code is no value, with or without a frequency.
        assert!(normalize_value("0603 100MHz", "FB").is_none());
        assert!(normalize_value("0603", "FB").is_none());
    }

    #[test]
    fn one_value_quoted_at_two_frequencies_stays_null() {
        // Two points for one value is a sweep, not a rating, and taking
        // either one by position would make the reading depend on the order
        // they were written in.
        assert!(normalize_value("1kΩ 100MHz 200MHz", "FB").is_none());
        assert!(normalize_value("1kΩ@100MHz 200MHz", "FB").is_none());
        // The same point twice is not a disagreement, however it is spelled.
        let v = normalize_value("1kΩ@100MHz 100Mhz", "FB").expect("should parse");
        assert_eq!(v.canonical, "1kΩ@100MHz");
    }

    #[test]
    fn a_frequency_reads_the_same_however_it_is_spelled() {
        // One point, six spellings, one identity. A lowercase "m" is mega
        // here: a bead rated in millihertz does not exist, and splitting the
        // part in two is the cost of pretending the ambiguity matters.
        for raw in [
            "1kΩ@100MHz",
            "1kΩ@100Mhz",
            "1kΩ@100mhz",
            "1kΩ@100MHZ",
            "1kΩ@100M",
            "1kΩ@100MHz/1.5A",
        ] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
            assert_eq!(v.at_frequency.as_deref(), Some("100MHz"), "{raw}");
        }
    }

    #[test]
    fn a_mark_that_is_not_a_frequency_does_not_become_one() {
        // "@25C" is a temperature and "@x" is a typo; neither says the value
        // is an impedance at a frequency, and neither says it is not. The
        // value reads, without an at_frequency that would claim a rating
        // point the string never gave.
        for raw in ["1kΩ@25C", "1kΩ@x", "1kΩ@"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ", "{raw}");
            assert!(v.at_frequency.is_none(), "{raw}");
        }
    }

    #[test]
    fn a_frequency_drops_the_punctuation_around_it() {
        // "100MHz," and "100MHz" are the same frequency; letting the comma
        // through would make them different parts.
        for raw in ["1kΩ@100MHz, 0.5A", "0.5A, 1kΩ@100MHz", "1kΩ@100MHz@x"] {
            let v = normalize_value(raw, "FB").unwrap_or_else(|| panic!("{raw} should parse"));
            assert_eq!(v.canonical, "1kΩ@100MHz", "{raw}");
            assert_eq!(v.at_frequency.as_deref(), Some("100MHz"), "{raw}");
        }
    }

    #[test]
    fn a_bead_with_no_readable_value_stays_null() {
        assert!(normalize_value("BLM18PG121SN1D", "FB").is_none());
        assert!(normalize_value("FerriteBead", "FB").is_none());
    }

    #[test]
    fn presence_flag_absent_key_is_false() {
        let props: HashMap<String, Option<String>> = HashMap::new();
        assert!(!presence_flag(&props, "dnp"));
    }

    #[test]
    fn presence_flag_valueless_key_is_true() {
        // How KiCad actually spells it: the name is emitted, with no value,
        // and only for the parts that carry the flag (issue #15).
        let mut props: HashMap<String, Option<String>> = HashMap::new();
        props.insert("dnp".to_string(), None);
        assert!(presence_flag(&props, "dnp"));
    }

    #[test]
    fn presence_flag_honours_an_explicit_value() {
        let mut props: HashMap<String, Option<String>> = HashMap::new();
        props.insert("dnp".to_string(), Some("yes".to_string()));
        assert!(presence_flag(&props, "dnp"));

        for falsy in ["no", "false", "0", "", "  "] {
            props.insert("dnp".to_string(), Some(falsy.to_string()));
            assert!(!presence_flag(&props, "dnp"), "{falsy:?} should be falsy");
        }
    }

    #[test]
    fn presence_flags_are_independent() {
        let mut props: HashMap<String, Option<String>> = HashMap::new();
        props.insert("dnp".to_string(), None);
        assert!(presence_flag(&props, "dnp"));
        assert!(!presence_flag(&props, "exclude_from_bom"));
    }

    /// Every bead value string quoted in the review of PR #27, pinned to its
    /// agreed reading. A rule change that moves any of them fails here, before
    /// a push, rather than in the next review round. See the file's header
    /// for how a row may change.
    #[test]
    fn bead_corpus_reads_as_agreed() {
        let corpus = include_str!("../tests/bead_corpus.tsv");
        let mut wrong = Vec::new();
        let mut rows = 0;
        for line in corpus.lines().map(str::trim_end).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (raw, want) = line.split_once('\t').unwrap_or_else(|| panic!("no tab in corpus row {line:?}"));
            let want = (want != "null").then_some(want);
            let got = normalize_value(raw, "FB").map(|v| v.canonical);
            if got.as_deref() != want {
                wrong.push(format!("{raw:?}: want {want:?}, got {got:?}"));
            }
            rows += 1;
        }
        assert!(rows > 0, "empty corpus");
        assert!(wrong.is_empty(), "{} of {rows} corpus rows changed:\n{}", wrong.len(), wrong.join("\n"));
    }
}

#[cfg(test)]
mod endpoint_class_tests {
    use super::{is_endpoint_class, is_passive_passthrough};

    /// Pin count for the cases where the class alone settles it.
    const ANY: usize = 2;

    #[test]
    fn two_pin_passives_and_probes_are_not_endpoints() {
        for class in ["R", "L", "C", "FB"] {
            assert!(
                !is_endpoint_class(class, 2),
                "2-pin {class} should not be an endpoint"
            );
        }
        for class in ["TP", "H", "MH", "FID"] {
            assert!(
                !is_endpoint_class(class, ANY),
                "{class} should not be an endpoint"
            );
        }
    }

    #[test]
    fn multi_terminal_passives_are_endpoints() {
        // A 4-pin common-mode choke, a 3-terminal EMI filter and a resistor
        // network all wear a passive prefix and are not passthroughs: `walk`
        // reports them under `endpoints`, so the predicate must agree or the
        // same response calls their net a dead end. See issue #13.
        for (class, pins) in [("L", 4), ("C", 3), ("R", 8), ("FB", 4)] {
            assert!(
                is_endpoint_class(class, pins),
                "{pins}-pin {class} should be an endpoint"
            );
        }
    }

    #[test]
    fn a_one_pin_passive_is_an_endpoint() {
        // Anomalous rather than inert — better surfaced than swallowed.
        assert!(is_endpoint_class("R", 1));
    }

    #[test]
    fn probes_are_inert_at_any_pin_count() {
        // Deliberate: the pin-count gate applies to passive prefixes only. It
        // costs us Connector's 3-pin `CoaxialSwitch_Testpoint`, which is a
        // real RF endpoint wearing a "TP" prefix — see PROBE_CLASSES.
        for pins in [1, 2, 3] {
            assert!(!is_endpoint_class("TP", pins));
        }
    }

    #[test]
    fn passthrough_and_endpoint_are_exact_complements_for_passives() {
        // The bug this pairing exists to prevent: `walk` branching on one rule
        // and the dead-end reason on another, so a response lists a part under
        // `endpoints` and calls its net "no active endpoint" in the same
        // breath.
        for class in ["R", "L", "C", "FB"] {
            for pins in [1, 2, 3, 4, 8] {
                assert_eq!(
                    is_passive_passthrough(class, pins),
                    !is_endpoint_class(class, pins),
                    "{pins}-pin {class}"
                );
            }
        }
    }

    #[test]
    fn mk_is_a_microphone_not_a_mechanical_part() {
        // KiCad hands "MK" to microphones (Device: Microphone*, all of
        // Sensor_Audio). A PDM mic biased by an R/C is precisely the #13
        // shape: deny-listing it puts its net back in `stub` and turns a walk
        // onto it into a dead end.
        assert!(is_endpoint_class("MK", ANY));
    }

    #[test]
    fn ics_and_connectors_are_endpoints() {
        for class in ["U", "J", "P", "CN", "RJ", "USB"] {
            assert!(
                is_endpoint_class(class, ANY),
                "{class} should be an endpoint"
            );
        }
    }

    #[test]
    fn the_classes_issue_13_reported_are_endpoints() {
        // RJ45 jack, Ethernet magnetics, crystal — nets landing on these were
        // reported as stubs with ic_pin_count 0.
        for class in ["RJ", "T", "X"] {
            assert!(
                is_endpoint_class(class, ANY),
                "{class} should be an endpoint"
            );
        }
    }

    #[test]
    fn an_unknown_class_is_assumed_to_be_a_real_part() {
        // The whole point of the deny-list: a class the tool has never heard
        // of is far more likely a real part than a probe point.
        for class in ["K", "BZ", "ANT", "MOD", "ISO"] {
            assert!(
                is_endpoint_class(class, ANY),
                "{class} should be an endpoint"
            );
        }
    }
}

#[cfg(test)]
mod net_hierarchy_tests {
    use super::{sheet_path_set, Design};
    use std::collections::HashSet;

    fn sheets(paths: &[&str]) -> HashSet<String> {
        sheet_path_set(paths.iter().copied())
    }

    #[test]
    fn splits_a_net_name_at_the_sheet_it_names() {
        let h = Design::net_hierarchy_in("/Power/EN", &sheets(&["/Power/"]));
        assert!(h.rooted);
        assert_eq!(h.local_name, "EN");
        assert_eq!(h.sheet_path.as_deref(), Some("/Power"));
        assert_eq!(h.depth, 1);
        assert_eq!(h.scope_hint, "hierarchical");
    }

    /// Issue #16: a label named after a dual-function pin. The slash is part
    /// of the label, and the sheet it would otherwise imply does not exist.
    #[test]
    fn keeps_a_slash_that_belongs_to_the_label() {
        let h = Design::net_hierarchy_in("/Ethernet/LED1/REGOFF", &sheets(&["/Ethernet/"]));
        assert_eq!(h.local_name, "LED1/REGOFF");
        assert_eq!(h.sheet_path.as_deref(), Some("/Ethernet"));
        assert_eq!(h.depth, 1);
    }

    #[test]
    fn prefers_the_deepest_sheet_that_matches() {
        let h = Design::net_hierarchy_in(
            "/Ethernet/Magnetics/CT",
            &sheets(&["/Ethernet/", "/Ethernet/Magnetics/"]),
        );
        assert_eq!(h.local_name, "CT");
        assert_eq!(h.sheet_path.as_deref(), Some("/Ethernet/Magnetics"));
        assert_eq!(h.depth, 2);
    }

    /// A sheet holding nothing but sub-sheets still names nets, so
    /// `sheet_path_set` keeps the ancestors of every path it is given.
    #[test]
    fn a_sheet_known_only_as_an_ancestor_still_splits() {
        let h = Design::net_hierarchy_in("/Ethernet/RST", &sheets(&["/Ethernet/Magnetics/"]));
        assert_eq!(h.local_name, "RST");
        assert_eq!(h.sheet_path.as_deref(), Some("/Ethernet"));
        assert_eq!(h.depth, 1);
    }

    /// The whole point of validating: never name a sheet the design has not
    /// got. A prefix that matches nothing leaves the name whole.
    #[test]
    fn an_unknown_prefix_reads_as_a_flat_name() {
        let h = Design::net_hierarchy_in("/LED1/REGOFF", &sheets(&["/Ethernet/"]));
        assert_eq!(h.local_name, "LED1/REGOFF");
        assert_eq!(h.sheet_path, None);
        assert_eq!(h.depth, 0);
        // Still rooted as written — `scope_hint` reports the name's shape,
        // which is not what changed here.
        assert!(h.rooted);
        assert_eq!(h.scope_hint, "hierarchical");
    }

    #[test]
    fn a_flat_name_has_no_hierarchy() {
        let h = Design::net_hierarchy_in("GND", &sheets(&["/Power/"]));
        assert!(!h.rooted);
        assert_eq!(h.local_name, "GND");
        assert_eq!(h.sheet_path, None);
        assert_eq!(h.depth, 0);
        assert_eq!(h.scope_hint, "flat");
    }

    /// KiCad's own auto-generated names carry a slash-bearing pin name
    /// unescaped by the time they reach here ("Net-(U2A-NINT/REFCLKO)").
    #[test]
    fn an_autogenerated_name_is_not_split() {
        let h = Design::net_hierarchy_in("Net-(U2A-NINT/REFCLKO)", &sheets(&["/Ethernet/"]));
        assert_eq!(h.local_name, "Net-(U2A-NINT/REFCLKO)");
        assert_eq!(h.sheet_path, None);
        assert_eq!(h.depth, 0);
    }

    /// The sheet path in a net name and the one in the design header come
    /// from the same schematic, but nothing in the format guarantees their
    /// case matches, and a case difference must not cost a net its hierarchy.
    #[test]
    fn matching_ignores_case() {
        let h = Design::net_hierarchy_in("/POWER/EN", &sheets(&["/Power/"]));
        assert_eq!(h.sheet_path.as_deref(), Some("/POWER"));
        assert_eq!(h.local_name, "EN");
    }

    /// The root sheet is in the set as "/", which no segment prefix can
    /// produce — a net on the root sheet is flat, and stays flat.
    #[test]
    fn the_root_sheet_is_not_a_split_point() {
        let h = Design::net_hierarchy_in("/SENSIN1", &sheets(&["/"]));
        assert_eq!(h.local_name, "SENSIN1");
        assert_eq!(h.sheet_path, None);
        assert_eq!(h.depth, 0);
    }

    /// A sheet whose name is a prefix of another's must not claim its nets.
    #[test]
    fn a_sheet_name_is_matched_whole_not_as_a_prefix() {
        let h = Design::net_hierarchy_in("/PowerAux/EN", &sheets(&["/Power/"]));
        assert_eq!(h.local_name, "PowerAux/EN");
        assert_eq!(h.sheet_path, None);
    }
}

#[cfg(test)]
mod filter_nets_rail_tests {
    use super::*;

    /// A design of one net per entry: `(net name, [(refdes, sheet, pin type)])`.
    /// Enough to exercise `filter_nets` end to end — every part is single-pin,
    /// which `rail_score` does not care about (it reads pin types, refdes class
    /// and fanout).
    fn design_of(nets: &[(&str, &[(&str, &str, &str)])]) -> Design {
        let mut design = Design {
            components: Vec::new(),
            pins: Vec::new(),
            nets: Vec::new(),
            component_map: HashMap::new(),
            pin_map: HashMap::new(),
            net_map: HashMap::new(),
            sheet_paths: HashSet::new(),
        };

        for (code, (name, members)) in nets.iter().enumerate() {
            let net_id = NetId(design.nets.len());
            let mut pin_ids: Vec<PinId> = Vec::new();
            let mut pin_types: HashMap<String, i32> = HashMap::new();

            for (refdes, sheet, pin_type) in members.iter() {
                let comp_id = match design.component_map.get(*refdes) {
                    Some(id) => CompId(id.0),
                    None => {
                        let id = CompId(design.components.len());
                        design.components.push(Component {
                            id: CompId(id.0),
                            refdes: refdes.to_string(),
                            value: String::new(),
                            value_norm: None,
                            footprint: None,
                            description: None,
                            sheet: Some(sheet.to_string()),
                            dnp: false,
                            exclude_from_bom: false,
                            properties: HashMap::new(),
                            pins: Vec::new(),
                        });
                        design.component_map.insert(refdes.to_string(), CompId(id.0));
                        id
                    }
                };

                let pin_id = PinId(design.pins.len());
                let number = (design.component(&comp_id).pins.len() + 1).to_string();
                design.pins.push(Pin {
                    id: PinId(pin_id.0),
                    comp: CompId(comp_id.0),
                    number: number.clone(),
                    name: None,
                    pin_type: Some(pin_type.to_string()),
                    net: Some(NetId(net_id.0)),
                });
                design.pin_map.insert(format!("{refdes}:{number}"), PinId(pin_id.0));
                design.components[comp_id.0].pins.push(PinId(pin_id.0));
                pin_ids.push(pin_id);
                *pin_types.entry(pin_type.to_string()).or_insert(0) += 1;
            }

            design.nets.push(Net {
                id: net_id,
                code: code + 1,
                name: name.to_string(),
                pins: pin_ids,
                pin_types,
            });
            design.net_map.insert(name.to_string(), NetId(design.nets.len() - 1));
        }

        let sheets: Vec<&str> = design.components
            .iter()
            .filter_map(|c| c.sheet.as_deref())
            .collect();
        design.sheet_paths = sheet_path_set(sheets.into_iter());
        design
    }

    /// The design from issue #4 in miniature: GND is the highest-fanout net on
    /// every sheet, and the sensor sheet's own signals are small.
    fn sensor_design() -> Design {
        design_of(&[
            ("GND", &[
                ("U1", "/Sensor1/", "power_in"), ("C1", "/Sensor1/", "passive"),
                ("C2", "/Sensor1/", "passive"), ("U2", "/MCU/", "power_in"),
                ("C3", "/MCU/", "passive"), ("C4", "/MCU/", "passive"),
                ("C5", "/MCU/", "passive"),
            ]),
            ("+3V3", &[
                ("U1", "/Sensor1/", "power_in"), ("U2", "/MCU/", "power_in"),
                ("C6", "/MCU/", "passive"),
            ]),
            ("/SENSOUT", &[("U1", "/Sensor1/", "output"), ("U2", "/MCU/", "input")]),
            ("/SENSIN", &[("U1", "/Sensor1/", "input"), ("R1", "/Sensor1/", "passive")]),
        ])
    }

    fn rows(json: &str) -> Vec<serde_json::Value> {
        let parsed: serde_json::Value = serde_json::from_str(json).expect("valid JSON");
        parsed["rows"].as_array().expect("rows").clone()
    }

    fn names(json: &str) -> Vec<String> {
        rows(json).iter().map(|r| r["name"].as_str().unwrap().to_string()).collect()
    }

    /// The issue itself: fanout-descending order used to put GND and +3V3 above
    /// every signal on the sheet. They are still on the page — last.
    #[test]
    fn rails_sort_last_under_a_subsystem_filter() {
        let design = sensor_design();
        let json = design.filter_nets(None, Some("Sensor1"), true, 50, 0).expect("filter_nets");
        assert_eq!(names(&json), ["/SENSIN", "/SENSOUT", "GND", "+3V3"]);
        // Demoted, never dropped — "does GND reach this sheet" stays answerable.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total"], 4);
        assert!(parsed["rail_note"].as_str().unwrap().starts_with("2 power/ground rail(s)"));
    }

    /// Alphabetical order is a presentation choice, not a different question:
    /// the rails go last there too, sorted among themselves by name.
    #[test]
    fn the_demotion_survives_the_alphabetical_sort() {
        let design = sensor_design();
        let json = design.filter_nets(None, Some("Sensor1"), false, 50, 0).expect("filter_nets");
        assert_eq!(names(&json), ["/SENSIN", "/SENSOUT", "+3V3", "GND"]);
    }

    /// Without a subsystem filter the question is about the whole design, where
    /// a high-fanout rail on top is the right answer — and the row stays compact.
    #[test]
    fn an_unfiltered_page_is_untouched() {
        let design = sensor_design();
        let json = design.filter_nets(None, None, true, 50, 0).expect("filter_nets");
        assert_eq!(names(&json), ["GND", "+3V3", "/SENSIN", "/SENSOUT"]);
        assert!(!json.contains("rail_score"), "{json}");
        assert!(!json.contains("rail_note"), "{json}");
    }

    /// A name search for a rail is not a subsystem query either.
    #[test]
    fn a_name_filter_alone_does_not_demote() {
        let design = sensor_design();
        let json = design.filter_nets(Some("gnd"), None, true, 50, 0).expect("filter_nets");
        assert_eq!(names(&json), ["GND"]);
        assert!(!json.contains("rail_note"), "{json}");
    }

    /// Paging is applied after the demotion, so page one is all signal: the
    /// rails are pushed onto the last page rather than eating the first.
    #[test]
    fn the_demotion_happens_before_pagination() {
        let design = sensor_design();
        let json = design.filter_nets(None, Some("Sensor1"), true, 2, 0).expect("filter_nets");
        assert_eq!(names(&json), ["/SENSIN", "/SENSOUT"]);
        // The note counts every demoted rail, including the ones off this page.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["rail_note"].as_str().unwrap().starts_with("2 power/ground rail(s)"));

        let json = design.filter_nets(None, Some("Sensor1"), true, 2, 2).expect("filter_nets");
        assert_eq!(names(&json), ["GND", "+3V3"]);
    }

    /// The score a row reports is the score it was ranked by. A raw 0.4975
    /// (0.45·0.3 power pins + 0.30 name + 0.25·0.25 caps — netdaq's `PGND`)
    /// prints as `0.5`, so it has to be demoted like one: reporting a rounded
    /// score while ranking on the raw one left a row reading exactly the
    /// threshold sitting on top of a page whose note said rails sort last.
    #[test]
    fn a_score_that_rounds_up_to_the_threshold_is_demoted_with_it() {
        let mut pgnd: Vec<(&str, &str, &str)> = Vec::new();
        for refdes in ["U1", "U2", "U3", "U4", "U5", "U6"] {
            pgnd.push((refdes, "/PoE/", "power_in"));      // 6/20 power pins
        }
        for refdes in ["C1", "C2", "C3", "C4", "C5"] {
            pgnd.push((refdes, "/PoE/", "passive"));       // 5/20 capacitors
        }
        for refdes in ["R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9"] {
            pgnd.push((refdes, "/PoE/", "passive"));
        }
        // Fanout is exactly 20, just under the >20 the fanout boost needs, so
        // the score stays on the low side of the threshold.
        let design = design_of(&[
            ("PGND", &pgnd),
            ("/POE_SW", &[("U1", "/PoE/", "output"), ("U7", "/PoE/", "input")]),
        ]);

        let json = design.filter_nets(None, Some("PoE"), true, 50, 0).expect("filter_nets");
        // Highest fanout by a wide margin, and still last.
        assert_eq!(names(&json), ["/POE_SW", "PGND"]);
        assert_eq!(rows(&json)[1]["rail_score"], 0.5);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["rail_note"].as_str().unwrap().contains("PGND"), "{json}");
    }

    /// The note has to survive a page that stops short of the rails: naming
    /// them and the offset they start at is the only thing a caller looking at
    /// five rail-free rows can act on.
    #[test]
    fn the_note_names_the_demoted_nets_and_where_they_start() {
        let design = sensor_design();

        let json = design.filter_nets(None, Some("Sensor1"), true, 50, 0).expect("filter_nets");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let note = parsed["rail_note"].as_str().unwrap();
        // Named in the order they were demoted to, not the order they matched.
        assert!(note.contains("2 power/ground rail(s) sorted last: GND, +3V3."), "{note}");
        assert!(note.contains("final 2 of 4 matches, from offset 2"), "{note}");

        // Same note on a page that shows none of them — the offset is the
        // caller's way back to the rows the note is about.
        let json = design.filter_nets(None, Some("Sensor1"), true, 2, 0).expect("filter_nets");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let note = parsed["rail_note"].as_str().unwrap();
        assert!(note.contains("GND, +3V3"), "{note}");
        assert!(note.contains("from offset 2"), "{note}");
    }

    /// Every row under a subsystem filter carries the score it was ranked by,
    /// so a caller can see why a net landed where it did — and a sheet with no
    /// rail on it gets no note.
    #[test]
    fn scored_rows_report_their_score_and_a_rail_free_sheet_gets_no_note() {
        let design = design_of(&[
            ("/SENSOUT", &[("U1", "/Sensor1/", "output"), ("U2", "/MCU/", "input")]),
        ]);
        let json = design.filter_nets(None, Some("Sensor1"), true, 50, 0).expect("filter_nets");
        assert_eq!(rows(&json)[0]["rail_score"], 0.0);
        assert!(!json.contains("rail_note"), "{json}");
    }
}

#[cfg(test)]
mod walk_topology_tests {
    use super::*;

    fn fixture() -> Design {
        let data = include_str!("../tests/fixture.net").to_string();
        let netlist = crate::parser::kicad_parser::parse_netlist(&data).expect("fixture parses");
        Design::from_netlist(netlist).expect("fixture builds")
    }

    fn walk(design: &Design, start: &str, max_depth: u32, topology: bool) -> serde_json::Value {
        capped(design, start, max_depth, topology, 200)
    }

    fn capped(
        design: &Design,
        start: &str,
        max_depth: u32,
        topology: bool,
        max_parts: u32,
    ) -> serde_json::Value {
        let json = design.walk(start, max_depth, 50, true, topology, max_parts).expect("walk");
        serde_json::from_str(&json).expect("valid JSON")
    }

    fn refdes(v: &serde_json::Value) -> Vec<String> {
        v["topology"]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["refdes"].as_str().unwrap().into())
            .collect()
    }

    /// Under the cap, nothing is cut and the counts say so.
    #[test]
    fn an_uncapped_graph_reports_no_cut() {
        let v = walk(&fixture(), "/SENSIN1", 4, true);
        assert_eq!(v["topology"]["truncated"], false);
        assert_eq!(v["topology"]["omitted_parts"], 0);
        assert_eq!(v["topology"]["omitted_nets"], 0);
    }

    /// Issue #34: `max_topology_parts` caps the graph, nearest parts first,
    /// series parts before endpoints at the same distance. A net only cut
    /// parts reach is left out, so no node hangs loose.
    #[test]
    fn the_cap_keeps_the_nearest_parts_and_their_nets() {
        let d = fixture();
        let full = walk(&d, "/SENSIN1", 4, true);
        let all = refdes(&full);
        // R11 (series) and J2 (endpoint) both sit at distance 0.
        assert_eq!(&all[..2], ["R11", "J2"], "{full}");
        let sens = || {
            vec![
                ("/SENSIN1".to_string(), "expanded".to_string()),
                ("/SENSMID1".to_string(), "expanded".to_string()),
            ]
        };

        let one = capped(&d, "/SENSIN1", 4, true, 1);
        assert_eq!(refdes(&one), ["R11"]);
        assert_eq!(nets(&one), sens());
        assert_eq!(one["topology"]["truncated"], true);
        assert_eq!(one["topology"]["omitted_parts"], all.len() - 1);
        assert_eq!(one["topology"]["omitted_nets"], 1);
        // J2 was cut from /SENSIN1; R21, C11 and TP11 from /SENSMID1.
        assert_eq!(one["topology"]["nets"][0]["omitted_parts"], 1, "{one}");
        assert_eq!(one["topology"]["nets"][1]["omitted_parts"], 3, "{one}");

        let two = capped(&d, "/SENSIN1", 4, true, 2);
        assert_eq!(refdes(&two), ["R11", "J2"]);
        assert_eq!(nets(&two), sens());
        assert_eq!(pins(part(&two, "J2").unwrap()), [pn("1", "/SENSIN1")]);
        assert_eq!(two["topology"]["omitted_nets"], 1);
        // Nothing cut on /SENSIN1 any more: the field is left out.
        assert!(two["topology"]["nets"][0].get("omitted_parts").is_none(), "{two}");

        // The endpoint list is not touched by the topology cap.
        assert_eq!(one["endpoints"], full["endpoints"]);
    }

    /// Issue #35: `max_endpoints` caps the `endpoints` list only. The graph
    /// is the same as with no endpoint cap, so an "expanded" net lists every
    /// part on it, and `max_topology_parts` still applies on top.
    #[test]
    fn max_endpoints_does_not_cut_the_graph() {
        let d = fixture();
        let run = |start: &str, max_endpoints: u32, max_parts: u32| -> serde_json::Value {
            let json = d.walk(start, 4, max_endpoints, true, true, max_parts).expect("walk");
            serde_json::from_str(&json).expect("valid JSON")
        };
        for start in ["/SENSIN1", "GND"] {
            let full = run(start, 50, 200);
            let end_parts: HashSet<&str> = full["endpoints"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["component"]["refdes"].as_str().unwrap())
                .collect();
            assert!(end_parts.len() > 1, "{start}: fixture needs more than one endpoint part");
            let one = run(start, 1, 200);
            assert_eq!(one["endpoints"].as_array().unwrap().len(), 1, "{start}");
            assert_eq!(one["truncated"], true, "{start}");
            assert_eq!(one["topology"], full["topology"], "{start}");
            // No endpoint listed: the graph comes from dropped parts alone.
            let none = run(start, 0, 200);
            assert!(none["endpoints"].as_array().unwrap().is_empty(), "{start}");
            assert_eq!(none["topology"], full["topology"], "{start}");
            // Both caps at once: the graph cap counts the dropped endpoints.
            let n_parts = refdes(&full).len();
            let both = run(start, 1, 1);
            assert_eq!(refdes(&both).len(), 1, "{start}");
            assert_eq!(both["topology"]["omitted_parts"], n_parts - 1, "{start}");
        }
    }

    /// The start part is kept first, whatever the cap (0 is read as 1).
    #[test]
    fn the_start_part_is_kept_first() {
        for cap in [0, 1] {
            let v = capped(&fixture(), "J2:1", 4, true, cap);
            assert_eq!(refdes(&v), ["J2"], "cap {cap}");
            assert_eq!(v["topology"]["parts"][0]["role"], "start");
            assert_eq!(nets(&v), [("/SENSIN1".into(), "expanded".into())]);
        }
    }

    /// At every cap, every pin's net is a listed node and every listed node
    /// is the start net or carries a pin of a listed part, and the counts add
    /// up to the uncapped graph.
    #[test]
    fn every_cap_leaves_no_loose_net_or_pin() {
        let d = fixture();
        for (start, start_net) in [("/SENSIN1", "/SENSIN1"), ("J2:1", "/SENSIN1"), ("GND", "GND")] {
            let full = walk(&d, start, 4, true);
            let n_parts = refdes(&full).len();
            let n_nets = nets(&full).len();
            for cap in 1..=n_parts as u32 + 1 {
                let v = capped(&d, start, 4, true, cap);
                let ctx = format!("{start} cap {cap}");
                let listed: HashSet<String> = nets(&v).into_iter().map(|(n, _)| n).collect();
                let parts = v["topology"]["parts"].as_array().unwrap();
                let mut used: HashSet<String> = HashSet::new();
                for p in parts {
                    for (_, net) in pins(p) {
                        if let Some(net) = net {
                            assert!(listed.contains(&net), "{ctx}: pin on unlisted {net}");
                            used.insert(net);
                        }
                    }
                }
                for n in &listed {
                    assert!(n == start_net || used.contains(n), "{ctx}: loose net {n}");
                }
                let t = &v["topology"];
                assert_eq!(parts.len() + t["omitted_parts"].as_u64().unwrap() as usize, n_parts, "{ctx}");
                assert_eq!(listed.len() + t["omitted_nets"].as_u64().unwrap() as usize, n_nets, "{ctx}");
                assert_eq!(t["truncated"], cap < n_parts as u32, "{ctx}");
                let net_cut: u64 =
                    t["nets"].as_array().unwrap().iter().map(|n| n["omitted_parts"].as_u64().unwrap_or(0)).sum();
                assert!(
                    t["nets"].as_array().unwrap().iter().all(|n| n["omitted_parts"] != 0),
                    "{ctx}: a zero per-net omitted_parts was serialized"
                );
                assert_eq!(net_cut > 0, cap < n_parts as u32, "{ctx}: per-net omitted_parts");
                // Recount each net's cut parts from the uncapped graph.
                let kept: HashSet<&str> = parts.iter().map(|p| p["refdes"].as_str().unwrap()).collect();
                for n in t["nets"].as_array().unwrap() {
                    let name = n["name"].as_str().unwrap();
                    let want = full["topology"]["parts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|p| !kept.contains(p["refdes"].as_str().unwrap()))
                        .filter(|p| pins(p).iter().any(|(_, net)| net.as_deref() == Some(name)))
                        .count() as u64;
                    assert_eq!(n["omitted_parts"].as_u64().unwrap_or(0), want, "{ctx}: {name}");
                }
            }
        }
    }

    /// (name, state) for every topology net, in order.
    fn nets(v: &serde_json::Value) -> Vec<(String, String)> {
        v["topology"]["nets"]
            .as_array()
            .expect("nets")
            .iter()
            .map(|n| (n["name"].as_str().unwrap().into(), n["state"].as_str().unwrap().into()))
            .collect()
    }

    fn part<'a>(v: &'a serde_json::Value, refdes: &str) -> Option<&'a serde_json::Value> {
        v["topology"]["parts"].as_array()?.iter().find(|p| p["refdes"] == refdes)
    }

    /// (pin, net) for one part.
    fn pins(p: &serde_json::Value) -> Vec<(String, Option<String>)> {
        p["pins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| (x["pin"].as_str().unwrap().into(), x["net"].as_str().map(String::from)))
            .collect()
    }

    fn pn(pin: &str, net: &str) -> (String, Option<String>) {
        (pin.into(), Some(net.into()))
    }

    /// (rail, via refdes chain) for every `rails_reached` entry.
    fn rail_arrivals(v: &serde_json::Value) -> Vec<(String, Vec<String>)> {
        v["rails_reached"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let via = r["via"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["refdes"].as_str().unwrap().to_string())
                    .collect();
                (r["net"].as_str().unwrap().to_string(), via)
            })
            .collect()
    }

    /// Issue #31: GND is reached over R21 and over C11. Both arrivals are
    /// listed, so the shunt cap is visible without `include_topology`.
    #[test]
    fn every_arrival_at_a_rail_is_listed() {
        let v = walk(&fixture(), "/SENSIN1", 4, false);
        assert_eq!(
            rail_arrivals(&v),
            [
                ("GND".to_string(), vec!["R11".to_string(), "R21".to_string()]),
                ("GND".to_string(), vec!["R11".to_string(), "C11".to_string()]),
            ]
        );
        assert_eq!(v["rails_reached"][0]["score"], v["rails_reached"][1]["score"]);
        assert_eq!(v["truncated"], false);
    }

    /// Repeat arrivals count against `max_endpoints`; the first arrival at a
    /// rail is kept, and the cut sets `truncated`. From J2:1 the only endpoint
    /// is TP11:1, so with a cap of 1 the endpoint list is not cut and
    /// `truncated` can only come from the rail cut.
    #[test]
    fn max_endpoints_cuts_repeat_rail_arrivals_only() {
        let run = |cap| {
            let json = fixture().walk("J2:1", 4, cap, true, false, 200).expect("walk");
            serde_json::from_str::<serde_json::Value>(&json).unwrap()
        };
        let cut = run(1);
        assert_eq!(cut["endpoints"].as_array().unwrap().len(), 1);
        assert_eq!(
            rail_arrivals(&cut),
            [("GND".to_string(), vec!["R11".to_string(), "R21".to_string()])]
        );
        assert_eq!(cut["truncated"], true);
        // Room for both arrivals: nothing is cut.
        let full = run(2);
        assert_eq!(rail_arrivals(&full).len(), 2);
        assert_eq!(full["truncated"], false);
    }

    #[test]
    fn without_the_flag_there_is_no_topology_key() {
        let v = walk(&fixture(), "/SENSIN1", 4, false);
        assert!(v.get("topology").is_none(), "{v}");
    }

    /// Issue #39: an edge is on a route when some simple s–t path uses it.
    #[test]
    fn edges_on_st_paths_keeps_routes_and_drops_side_branches() {
        // 0=s, 9=t. Two routes s-1-t and s-t, a dead-end branch 1-2-3, a
        // loop 1-4 (two parallel edges) that leads nowhere, and a self-loop.
        let edges = [(0, 1), (1, 9), (0, 9), (1, 2), (2, 3), (1, 4), (4, 1), (5, 5)];
        assert_eq!(
            edges_on_st_paths(&edges, 0, 9),
            [true, true, true, false, false, false, false, false]
        );
        // Parallel edges between s and t are both on a route.
        assert_eq!(edges_on_st_paths(&[(0, 9), (9, 0)], 0, 9), [true, true]);
        // A cycle s-1-2-s with t hanging off 2: the whole cycle is on a
        // route (s-1-2-t and s-2-t), t's own edge too.
        assert_eq!(edges_on_st_paths(&[(0, 1), (1, 2), (2, 0), (2, 9)], 0, 9), [true; 4]);
        // A chain of blocks: s=0 and t=9 joined through cut node 2. The loop
        // 2-6-7-2 after t's block is off the route.
        assert_eq!(
            edges_on_st_paths(&[(0, 1), (1, 2), (0, 2), (2, 9), (2, 6), (6, 7), (7, 2)], 0, 9),
            [true, true, true, true, false, false, false]
        );
        // t not reachable, and s == t: nothing.
        assert_eq!(edges_on_st_paths(&[(0, 1)], 0, 9), [false]);
        assert_eq!(edges_on_st_paths(&[(0, 1), (1, 0)], 0, 0), [false, false]);
    }

    /// Issue #39: route parts, shunts down to a rail, and everything else
    /// left out.
    #[test]
    fn path_subgraph_sorts_route_shunt_and_off() {
        use PathRole::*;
        // Nets: s=0, t=1, 2 = mid-shunt net, 3 = dead end, 100 = rail GND,
        // 101 = another rail, 50 = depth-limit net (not walked).
        let walked: HashSet<usize> = [0, 1, 2, 3].into();
        let rails: HashSet<usize> = [100, 101].into();
        let parts = [
            Some((0, 1)),     // route
            Some((1, 0)),     // parallel route
            Some((1, 100)),   // shunt cap straight to GND
            Some((0, 2)),     // RC to ground: first half
            Some((2, 101)),   //               second half
            Some((1, 3)),     // dead end
            Some((1, 50)),    // into a net the walk did not expand
            None,             // a pin with no net
            Some((100, 101)), // between two rails
        ];
        let (roles, route_nets) = path_subgraph(&parts, &walked, &rails, 0, 1);
        assert_eq!(roles, [Route, Route, Shunt, Shunt, Shunt, Off, Off, Off, Off]);
        assert_eq!(route_nets, [0, 1].into());

        // A rail-only path: t is GND. A part into GND is a route part, and
        // a part to the other rail is a shunt.
        let (roles, _) = path_subgraph(&[Some((0, 100)), Some((0, 101))], &walked, &rails, 0, 100);
        assert_eq!(roles, [Route, Shunt]);

        // s == t (same net): no route parts, shunts still found.
        let (roles, route_nets) = path_subgraph(&[Some((0, 100)), Some((0, 3))], &walked, &rails, 0, 0);
        assert_eq!(roles, [Shunt, Off]);
        assert_eq!(route_nets, [0].into());
    }

    fn path(design: &Design, from: &str, to: &str, topology: bool, cap: u32) -> serde_json::Value {
        let json = design.path_between(from, to, topology, cap).expect("path_between");
        serde_json::from_str(&json).expect("valid JSON")
    }

    /// Issue #39, the fixture case: J3:1 to U3:1 over R50 and R51 in
    /// parallel, with C50 to GND and TP51 on the route net. R53 and C51 are
    /// an RC shunt to GND; TP52 on its middle net /PATHC is kept too. R52
    /// and TP50 lead nowhere and are pruned.
    #[test]
    fn path_topology_keeps_both_routes_and_the_shunt() {
        let d = fixture();
        let v = path(&d, "J3:1", "U3:1", true, 200);
        assert_eq!(v["connected"], true);
        assert_eq!(
            refdes(&v),
            ["J3", "U3", "R50", "R51", "C50", "R53", "C51", "TP51", "TP52"],
            "{v}"
        );
        let roles: Vec<&str> = v["topology"]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            ["start", "endpoint", "series", "series", "series", "series", "series", "endpoint", "endpoint"]
        );
        assert_eq!(
            nets(&v),
            [
                ("/PATHA".into(), "expanded".into()),
                ("/PATHB".into(), "expanded".into()),
                ("GND".into(), "rail".into()),
                ("/PATHC".into(), "expanded".into()),
            ]
        );
        assert_eq!(pins(part(&v, "TP52").unwrap()), [pn("1", "/PATHC")]);
        assert_eq!(pins(part(&v, "R51").unwrap()), [pn("1", "/PATHA"), pn("2", "/PATHB")]);
        assert_eq!(pins(part(&v, "C50").unwrap()), [pn("1", "/PATHB"), pn("2", "GND")]);
        // End parts list only their pins on kept nets.
        assert_eq!(pins(part(&v, "J3").unwrap()), [pn("1", "/PATHA"), pn("2", "GND")]);
        assert_eq!(pins(part(&v, "U3").unwrap()), [pn("1", "/PATHB"), pn("11", "GND")]);
        assert_eq!(v["topology"]["truncated"], false);
        assert_eq!(v["topology"]["omitted_parts"], 0);
        assert_eq!(v["topology"]["omitted_nets"], 0);

        // The flat answer is the same with and without the flag.
        let flat = path(&d, "J3:1", "U3:1", false, 200);
        assert!(flat.get("topology").is_none(), "{flat}");
        let mut with = v.clone();
        with.as_object_mut().unwrap().remove("topology");
        assert_eq!(with, flat);
    }

    /// The cap keeps both end parts first, then route parts before shunts.
    #[test]
    fn path_topology_cap_keeps_the_ends_then_the_route() {
        let d = fixture();
        for cap in [0, 1, 2] {
            let v = path(&d, "J3:1", "U3:1", true, cap);
            assert_eq!(refdes(&v), ["J3", "U3"], "cap {cap}");
            assert_eq!(v["topology"]["truncated"], true);
            assert_eq!(v["topology"]["omitted_parts"], 7);
            // Only the two end nets are left; GND and /PATHC came in through
            // the shunts alone.
            let listed: Vec<String> = nets(&v).into_iter().map(|(n, _)| n).collect();
            assert_eq!(listed, ["/PATHA", "/PATHB"], "cap {cap}");
            assert_eq!(v["topology"]["omitted_nets"], 2);
        }
        let v = path(&d, "J3:1", "U3:1", true, 4);
        assert_eq!(refdes(&v), ["J3", "U3", "R50", "R51"]);
    }

    /// Same net, a rail-only path and no path.
    #[test]
    fn path_topology_edge_cases() {
        let d = fixture();
        // TP51:1 and U3:1 share /PATHB: no route parts, but the shunts and
        // TP52 are kept. R50 and R51 lead to /PATHA, which reaches neither
        // `to` nor a rail, so they and J3 are left out.
        let v = path(&d, "TP51:1", "U3:1", true, 200);
        assert_eq!(v["distance"], 0);
        assert_eq!(refdes(&v), ["TP51", "U3", "C50", "R53", "C51", "TP52"], "{v}");

        // Only via GND: the route runs over C50 into the rail.
        let v = path(&d, "J3:1", "U2:2", true, 200);
        assert_eq!(v["connected"], true, "{v}");
        assert!(v["note"].as_str().unwrap().contains("only via rail GND"), "{v}");
        let parts = refdes(&v);
        assert_eq!(&parts[..2], ["J3", "U2"]);
        assert!(parts.contains(&"C50".to_string()), "{v}");
        assert!(!parts.contains(&"R52".to_string()), "{v}");
        assert_eq!(pins(part(&v, "U2").unwrap()), [pn("2", "GND")]);

        // A passive `to` part is itself the last route edge: J3:1 to C50:2
        // runs over R50 or R51, then C50 into GND.
        let v = path(&d, "J3:1", "C50:2", true, 200);
        assert_eq!(v["connected"], true, "{v}");
        let parts = refdes(&v);
        assert_eq!(&parts[..4], ["J3", "C50", "R50", "R51"], "{v}");
        assert_eq!(part(&v, "C50").unwrap()["role"], "endpoint");
        assert_eq!(pins(part(&v, "C50").unwrap()), [pn("1", "/PATHB"), pn("2", "GND")]);

        // Same net, and it is a rail: just the two parts, not the rail's
        // whole fanout.
        let v = path(&d, "J3:2", "U2:2", true, 200);
        assert_eq!(v["distance"], 0, "{v}");
        assert_eq!(refdes(&v), ["J3", "U2"], "{v}");
        assert_eq!(nets(&v), [("GND".into(), "rail".into())]);
        assert_eq!(pins(part(&v, "J3").unwrap()), [pn("2", "GND")]);

        // Not connected: no topology.
        let v = path(&d, "J3:1", "U2:7", true, 200);
        assert_eq!(v["connected"], false);
        assert!(v.get("topology").is_none(), "{v}");
    }

    /// The case from #28: an RC to ground. The graph keeps C11 and the net on
    /// each of its pins.
    #[test]
    fn the_graph_keeps_the_shunt_cap_and_its_ground_side() {
        let v = walk(&fixture(), "/SENSIN1", 4, true);
        assert_eq!(
            nets(&v),
            [
                ("/SENSIN1".into(), "expanded".into()),
                ("/SENSMID1".into(), "expanded".into()),
                ("GND".into(), "rail".into()),
            ]
        );
        assert_eq!(v["topology"]["nets"][2]["rail"], true);

        let r11 = part(&v, "R11").expect("R11");
        assert_eq!(r11["role"], "series");
        assert_eq!(pins(r11), [pn("1", "/SENSIN1"), pn("2", "/SENSMID1")]);
        let c11 = part(&v, "C11").expect("C11");
        assert_eq!(c11["role"], "series");
        assert_eq!(pins(c11), [pn("1", "/SENSMID1"), pn("2", "GND")]);
        assert_eq!(pins(part(&v, "R21").expect("R21")), [pn("1", "/SENSMID1"), pn("2", "GND")]);

        assert_eq!(part(&v, "J2").expect("J2")["role"], "endpoint");
        assert_eq!(part(&v, "TP11").expect("TP11")["role"], "endpoint");
    }

    /// The start part is an edge too, but shows only the pins the walk reached:
    /// J2's pins 2 and 3 sit on other sensor channels.
    #[test]
    fn a_start_part_lists_only_pins_on_reached_nets() {
        let v = walk(&fixture(), "J2:1", 4, true);
        let parts = v["topology"]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["refdes"], "J2");
        assert_eq!(parts[0]["role"], "start");
        assert_eq!(pins(&parts[0]), [pn("1", "/SENSIN1")]);
        assert_eq!(parts.iter().filter(|p| p["refdes"] == "J2").count(), 1);
    }

    /// At the depth limit the far net is still a node, so the series part
    /// that hit the limit has both ends; nothing beyond it is listed.
    #[test]
    fn a_depth_limited_net_is_a_leaf_node() {
        let v = walk(&fixture(), "/SENSIN1", 0, true);
        assert_eq!(
            nets(&v),
            [
                ("/SENSIN1".into(), "expanded".into()),
                ("/SENSMID1".into(), "depth_limit".into()),
            ]
        );
        assert_eq!(pins(part(&v, "R11").expect("R11")), [pn("1", "/SENSIN1"), pn("2", "/SENSMID1")]);
        assert!(part(&v, "C11").is_none(), "{v}");
        assert!(part(&v, "TP11").is_none(), "{v}");
    }

    /// A net marked at the depth limit from one branch and popped from a
    /// shorter one ends up with the popped state; a popped state never goes
    /// back.
    #[test]
    fn a_popped_state_wins_and_is_final() {
        let mut t = TopoTrace::new(true);
        t.note_net(7, NetState::DepthLimit);
        t.note_net(7, NetState::Expanded);
        t.note_net(7, NetState::Dangling);
        t.note_net(8, NetState::Rail);
        t.note_net(8, NetState::DepthLimit);
        assert_eq!(t.nets, [(7, NetState::Expanded), (8, NetState::Rail)]);
    }
}
