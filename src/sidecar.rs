//! Read-only enrichment sidecar (issue #46): facts the netlist does not carry,
//! joined with the parsed design at startup and shown in `get_component`.
//!
//! Two files, both optional:
//! - the project file, `<netlist stem>.sidecar.json` next to the `.net`;
//! - the shared library, named by `$NETLIST_MCP_LIBRARY`.
//!
//! Each is a JSON object with up to three tables of entries:
//!
//! ```json
//! {
//!   "lib_id": {"Device:L_CMC": {"note": "common-mode choke"}},
//!   "mpn":    {"TLV9004IPWR": {"note": "quad op-amp"}},
//!   "refdes": {"U12": {"note": "channel B unused"}}
//! }
//! ```
//!
//! `refdes` entries belong to one board, so only the project file may hold
//! them. An entry is any JSON object; its fields are not interpreted here.
//!
//! A component matches by `lib_id` (raw, escape tokens kept: #51), by MPN (the
//! `MPN` property, else `value`) and by refdes, all exact. The matches are
//! merged field by field (top-level keys only) in this order, later wins:
//! library lib_id, project lib_id, library mpn, project mpn, project refdes.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::{Map, Value};

/// Environment variable naming the shared MPN / lib_id library file.
pub const LIBRARY_ENV: &str = "NETLIST_MCP_LIBRARY";

/// Which of the two files an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Library,
    Project,
}

/// The three key scopes, in ascending precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    LibId,
    Mpn,
    Refdes,
}

impl Scope {
    const ALL: [Scope; 3] = [Scope::LibId, Scope::Mpn, Scope::Refdes];

    fn table_name(self) -> &'static str {
        match self {
            Scope::LibId => "lib_id",
            Scope::Mpn => "mpn",
            Scope::Refdes => "refdes",
        }
    }
}

type Entries = BTreeMap<String, Map<String, Value>>;

/// One parsed sidecar file.
#[derive(Debug, Clone)]
pub struct SidecarFile {
    pub path: String,
    pub origin: Origin,
    lib_id: Entries,
    mpn: Entries,
    refdes: Entries,
}

impl SidecarFile {
    fn table(&self, scope: Scope) -> &Entries {
        match scope {
            Scope::LibId => &self.lib_id,
            Scope::Mpn => &self.mpn,
            Scope::Refdes => &self.refdes,
        }
    }

    fn table_mut(&mut self, scope: Scope) -> &mut Entries {
        match scope {
            Scope::LibId => &mut self.lib_id,
            Scope::Mpn => &mut self.mpn,
            Scope::Refdes => &mut self.refdes,
        }
    }

    /// Parse one file's text. Rejects an unknown top-level key (a typo would
    /// otherwise drop a whole table without a word), an entry that is not an
    /// object, and `refdes` entries in the library.
    pub fn parse(text: &str, path: &str, origin: Origin) -> anyhow::Result<SidecarFile> {
        let root: Value = serde_json::from_str(text)
            .with_context(|| format!("sidecar {path}: not valid JSON"))?;
        let Value::Object(root) = root else {
            bail!("sidecar {path}: top level must be a JSON object");
        };
        let mut file = SidecarFile {
            path: path.to_string(),
            origin,
            lib_id: Entries::new(),
            mpn: Entries::new(),
            refdes: Entries::new(),
        };
        for (key, table) in root {
            let scope = match key.as_str() {
                "lib_id" => Scope::LibId,
                "mpn" => Scope::Mpn,
                "refdes" => Scope::Refdes,
                other => bail!(
                    "sidecar {path}: unknown top-level key {other:?} (expected \"lib_id\", \"mpn\" or \"refdes\")"
                ),
            };
            if scope == Scope::Refdes && origin == Origin::Library {
                bail!(
                    "sidecar {path}: the shared library (${LIBRARY_ENV}) cannot hold \"refdes\" entries; put them in the project file"
                );
            }
            let Value::Object(table) = table else {
                bail!("sidecar {path}: \"{key}\" must be an object of entries");
            };
            let mut entries = Entries::new();
            for (k, entry) in table {
                let Value::Object(entry) = entry else {
                    bail!("sidecar {path}: entry {key}.{k:?} must be a JSON object");
                };
                entries.insert(k, entry);
            }
            *file.table_mut(scope) = entries;
        }
        Ok(file)
    }
}

/// A file the loader looked for, found or not. Reported by `design_overview`
/// so a user can see where the project file is expected.
#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub path: String,
    pub origin: Origin,
    pub found: bool,
}

/// All loaded sidecar files, library first.
#[derive(Debug, Clone, Default)]
pub struct Sidecar {
    files: Vec<SidecarFile>,
    probes: Vec<Probe>,
}

/// The identity a component is matched on.
pub struct Keys<'a> {
    pub refdes: &'a str,
    pub lib_id: &'a str,
    pub mpn: &'a str,
}

impl<'a> Keys<'a> {
    fn get(&self, scope: Scope) -> &'a str {
        match scope {
            Scope::LibId => self.lib_id,
            Scope::Mpn => self.mpn,
            Scope::Refdes => self.refdes,
        }
    }
}

/// One entry that matched, in the order it was merged.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Source {
    pub file: String,
    pub origin: Origin,
    pub scope: Scope,
    pub key: String,
}

/// The joined entry for one component.
#[derive(Debug, Clone, Serialize)]
pub struct Joined {
    /// The merged fields. A later source overrides an earlier one per
    /// top-level field.
    pub entry: Map<String, Value>,
    /// Every matching entry, lowest precedence first.
    pub sources: Vec<Source>,
}

/// Per-file summary for `design_overview`.
#[derive(Debug, Clone, Serialize)]
pub struct FileSummary {
    pub path: String,
    pub origin: Origin,
    pub entries: BTreeMap<&'static str, usize>,
    /// Keys that match no component in this design, capped at 50 — most
    /// often a typo, or (in the library) a part this board does not use.
    pub unmatched: Vec<String>,
    pub unmatched_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub probed: Vec<Probe>,
    pub files: Vec<FileSummary>,
}

impl Sidecar {
    /// Build from already-parsed files. Library files must come before
    /// project files; `load` does this.
    #[cfg(test)]
    pub fn from_files(files: Vec<SidecarFile>, probes: Vec<Probe>) -> Sidecar {
        Sidecar { files, probes }
    }

    /// The project file path for a netlist: same directory and stem,
    /// extension `sidecar.json` ("board.net" -> "board.sidecar.json").
    pub fn project_path(netlist: &Path) -> PathBuf {
        netlist.with_extension("sidecar.json")
    }

    /// Load the library (if `library` is given) and the project file next to
    /// `netlist`. A file that does not exist is skipped; a file that exists
    /// but cannot be read or parsed is an error.
    pub fn load(netlist: &Path, library: Option<&Path>) -> anyhow::Result<Sidecar> {
        let mut files = Vec::new();
        let mut probes = Vec::new();
        let project = Self::project_path(netlist);
        let candidates = library
            .map(|p| (p.to_path_buf(), Origin::Library))
            .into_iter()
            .chain(std::iter::once((project, Origin::Project)));
        for (path, origin) in candidates {
            let shown = path.display().to_string();
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    files.push(SidecarFile::parse(&text, &shown, origin)?);
                    probes.push(Probe { path: shown, origin, found: true });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    probes.push(Probe { path: shown, origin, found: false });
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("sidecar {shown}: cannot read"));
                }
            }
        }
        Ok(Sidecar { files, probes })
    }

    /// Join the entries that match `keys`, or None when nothing matches.
    pub fn join(&self, keys: &Keys) -> Option<Joined> {
        let mut entry = Map::new();
        let mut sources = Vec::new();
        for scope in Scope::ALL {
            let key = keys.get(scope);
            if key.is_empty() {
                continue;
            }
            for file in &self.files {
                if let Some(found) = file.table(scope).get(key) {
                    for (k, v) in found {
                        entry.insert(k.clone(), v.clone());
                    }
                    sources.push(Source {
                        file: file.path.clone(),
                        origin: file.origin,
                        scope,
                        key: key.to_string(),
                    });
                }
            }
        }
        if sources.is_empty() { None } else { Some(Joined { entry, sources }) }
    }

    /// Per-file entry counts and the keys that match no component. `all`
    /// lists every component's keys. An empty key is never present, since
    /// `join` never matches on one.
    pub fn summary<'a>(&self, all: impl Iterator<Item = Keys<'a>>) -> Summary {
        let mut present: [HashSet<&str>; 3] = Default::default();
        for k in all {
            for (i, scope) in Scope::ALL.into_iter().enumerate() {
                let key = k.get(scope);
                if !key.is_empty() {
                    present[i].insert(key);
                }
            }
        }
        let files = self.files.iter().map(|file| {
            let mut entries = BTreeMap::new();
            let mut unmatched = Vec::new();
            for (i, scope) in Scope::ALL.into_iter().enumerate() {
                let table = file.table(scope);
                if !table.is_empty() {
                    entries.insert(scope.table_name(), table.len());
                }
                unmatched.extend(
                    table.keys()
                        .filter(|k| !present[i].contains(k.as_str()))
                        .map(|k| format!("{}.{}", scope.table_name(), k)),
                );
            }
            let unmatched_count = unmatched.len();
            unmatched.truncate(50);
            FileSummary {
                path: file.path.clone(),
                origin: file.origin,
                entries,
                unmatched,
                unmatched_count,
            }
        }).collect();
        Summary { probed: self.probes.clone(), files }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(v: Value, origin: Origin) -> SidecarFile {
        SidecarFile::parse(&v.to_string(), "t.json", origin).expect("parses")
    }

    fn keys<'a>(refdes: &'a str, lib_id: &'a str, mpn: &'a str) -> Keys<'a> {
        Keys { refdes, lib_id, mpn }
    }

    #[test]
    fn missing_tables_are_empty() {
        let s = Sidecar::from_files(vec![file(json!({}), Origin::Project)], vec![]);
        assert!(s.join(&keys("U1", "Device:R", "10k")).is_none());
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let err = SidecarFile::parse(r#"{"mpns": {}}"#, "t.json", Origin::Project).unwrap_err();
        assert!(format!("{err:#}").contains("unknown top-level key \"mpns\""), "{err:#}");
    }

    #[test]
    fn rejects_non_object_entry() {
        let err = SidecarFile::parse(r#"{"mpn": {"X": 3}}"#, "t.json", Origin::Project).unwrap_err();
        assert!(format!("{err:#}").contains("must be a JSON object"), "{err:#}");
    }

    #[test]
    fn rejects_bad_json_and_non_object_root() {
        assert!(SidecarFile::parse("{", "t.json", Origin::Project).is_err());
        assert!(SidecarFile::parse("[]", "t.json", Origin::Project).is_err());
    }

    #[test]
    fn library_cannot_hold_refdes() {
        let err = SidecarFile::parse(r#"{"refdes": {"U1": {}}}"#, "lib.json", Origin::Library)
            .unwrap_err();
        assert!(format!("{err:#}").contains("cannot hold \"refdes\""), "{err:#}");
    }

    // Precedence, lowest first: library lib_id, project lib_id, library mpn,
    // project mpn, project refdes. Merge is per top-level field.
    #[test]
    fn join_merges_in_precedence_order() {
        let lib = file(json!({
            "lib_id": {"Amp:Quad": {"a": "lib-lib", "b": "lib-lib", "c": "lib-lib", "d": "lib-lib", "e": "lib-lib"}},
            "mpn": {"TLV9004": {"c": "lib-mpn", "d": "lib-mpn", "e": "lib-mpn"}}
        }), Origin::Library);
        let proj = file(json!({
            "lib_id": {"Amp:Quad": {"b": "proj-lib", "c": "proj-lib", "d": "proj-lib", "e": "proj-lib"}},
            "mpn": {"TLV9004": {"d": "proj-mpn", "e": "proj-mpn"}},
            "refdes": {"U3": {"e": "proj-refdes"}}
        }), Origin::Project);
        let s = Sidecar::from_files(vec![lib, proj], vec![]);
        let j = s.join(&keys("U3", "Amp:Quad", "TLV9004")).expect("matches");
        assert_eq!(Value::Object(j.entry), json!({
            "a": "lib-lib", "b": "proj-lib", "c": "lib-mpn", "d": "proj-mpn", "e": "proj-refdes"
        }));
        let order: Vec<(Origin, Scope)> = j.sources.iter().map(|s| (s.origin, s.scope)).collect();
        assert_eq!(order, vec![
            (Origin::Library, Scope::LibId),
            (Origin::Project, Scope::LibId),
            (Origin::Library, Scope::Mpn),
            (Origin::Project, Scope::Mpn),
            (Origin::Project, Scope::Refdes),
        ]);
    }

    // Merge is shallow: a refdes entry's object field replaces the MPN one
    // whole, it is not merged into it.
    #[test]
    fn join_is_shallow() {
        let proj = file(json!({
            "mpn": {"X": {"pins": {"1": "in", "2": "out"}}},
            "refdes": {"U1": {"pins": {"1": "nc"}}}
        }), Origin::Project);
        let s = Sidecar::from_files(vec![proj], vec![]);
        let j = s.join(&keys("U1", "", "X")).unwrap();
        assert_eq!(j.entry["pins"], json!({"1": "nc"}));
    }

    #[test]
    fn keys_match_exactly() {
        let proj = file(json!({"mpn": {"TLV9004": {"n": 1}}}), Origin::Project);
        let s = Sidecar::from_files(vec![proj], vec![]);
        assert!(s.join(&keys("U1", "", "tlv9004")).is_none());
        assert!(s.join(&keys("U1", "", "TLV9004IPWR")).is_none());
        // An empty key never matches, even an entry keyed "".
        let proj = file(json!({"mpn": {"": {"n": 1}}}), Origin::Project);
        let s = Sidecar::from_files(vec![proj], vec![]);
        assert!(s.join(&keys("U1", "", "")).is_none());
    }

    #[test]
    fn summary_lists_unmatched_keys() {
        let proj = file(json!({
            "mpn": {"TLV9004": {}, "NOPE": {}},
            "refdes": {"U1": {}, "U99": {}}
        }), Origin::Project);
        let s = Sidecar::from_files(vec![proj], vec![]);
        let sum = s.summary(std::iter::once(keys("U1", "Amp:Quad", "TLV9004")));
        let f = &sum.files[0];
        assert_eq!(f.entries.get("mpn"), Some(&2));
        assert_eq!(f.entries.get("lib_id"), None);
        assert_eq!(f.unmatched, vec!["mpn.NOPE", "refdes.U99"]);
        assert_eq!(f.unmatched_count, 2);
    }

    // An entry keyed "" can never join, so it is unmatched even when some
    // component has an empty key in that scope.
    #[test]
    fn summary_reports_empty_key_as_unmatched() {
        let proj = file(json!({"lib_id": {"": {}}}), Origin::Project);
        let s = Sidecar::from_files(vec![proj], vec![]);
        let sum = s.summary(std::iter::once(keys("U1", "", "X")));
        assert_eq!(sum.files[0].unmatched, vec!["lib_id."]);
    }

    #[test]
    fn project_path_replaces_extension() {
        assert_eq!(
            Sidecar::project_path(Path::new("/x/board.net")),
            PathBuf::from("/x/board.sidecar.json")
        );
        assert_eq!(
            Sidecar::project_path(Path::new("board")),
            PathBuf::from("board.sidecar.json")
        );
    }

    #[test]
    fn load_skips_missing_files() {
        let dir = std::env::temp_dir().join(format!("sidecar-test-{}", std::process::id()));
        let s = Sidecar::load(&dir.join("none.net"), Some(&dir.join("nolib.json"))).expect("ok");
        assert!(s.files.is_empty());
        assert_eq!(s.probes.len(), 2);
        assert!(s.probes.iter().all(|p| !p.found));
        assert_eq!(s.probes[0].origin, Origin::Library);
    }
}
