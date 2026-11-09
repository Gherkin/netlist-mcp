//! Enrichment sidecar (issue #46): facts the netlist does not carry, joined
//! with the parsed design at startup and shown in `get_component`. The query
//! tools only read it; `write_sidecar` (issue #48, see `write_file`) is the
//! one tool that changes it.
//!
//! Two files, both optional:
//! - the project file, `<netlist stem>.sidecar.json` next to the `.net`;
//! - the shared library, named by `$NETLIST_MCP_LIBRARY`. Unset or empty
//!   means no library; set to a path that does not exist stops startup (#54).
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
//! `MPN` property, else `value`) and by refdes, all exact after trimming the
//! component's keys (a key `write_file` stores is trimmed too). The matches are
//! merged field by field (top-level keys only) in this order, later wins:
//! library lib_id, project lib_id, library mpn, project mpn, project refdes.
//!
//! Writing (issue #48) changes one entry in one file. Another process (a
//! second server sharing the library, or a person in an editor) may change
//! the same file, so a write takes an OS lock on `<file>.lock`, re-reads the
//! file from disk, changes only its own entry, and renames a temp file over
//! the original. Entries written elsewhere are kept.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Environment variable naming the shared MPN / lib_id library file.
pub const LIBRARY_ENV: &str = "NETLIST_MCP_LIBRARY";

/// Which of the two files an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Library,
    Project,
}

/// The three key scopes, in ascending precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
        let mut file = SidecarFile::empty(path, origin);
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

    fn empty(path: &str, origin: Origin) -> SidecarFile {
        SidecarFile {
            path: path.to_string(),
            origin,
            lib_id: Entries::new(),
            mpn: Entries::new(),
            refdes: Entries::new(),
        }
    }

    /// The file as JSON text: tables in scope order, empty tables left out,
    /// keys sorted, two-space indent, trailing newline.
    fn to_json(&self) -> String {
        let mut root = Map::new();
        for scope in Scope::ALL {
            let table = self.table(scope);
            if !table.is_empty() {
                let entries = table.iter()
                    .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
                    .collect();
                root.insert(scope.table_name().to_string(), Value::Object(entries));
            }
        }
        let mut text = serde_json::to_string_pretty(&Value::Object(root))
            .expect("a serde_json::Value always serializes");
        text.push('\n');
        text
    }

    /// Apply one edit. Returns the entry as now stored, or None when it was
    /// removed (by `delete`, or because its last field was nulled out).
    fn apply(&mut self, edit: &Edit) -> anyhow::Result<Option<Map<String, Value>>> {
        let path = self.path.clone();
        let table = self.table_mut(edit.scope);
        if edit.delete {
            if table.remove(edit.key).is_none() {
                bail!(
                    "sidecar {path}: no {}.{:?} entry to delete",
                    edit.scope.table_name(), edit.key
                );
            }
            return Ok(None);
        }
        if !table.contains_key(edit.key) && edit.fields.values().all(Value::is_null) {
            bail!(
                "sidecar {path}: no {}.{:?} entry to remove fields from",
                edit.scope.table_name(), edit.key
            );
        }
        let entry = table.entry(edit.key.to_string()).or_default();
        for (k, v) in edit.fields {
            if v.is_null() {
                entry.remove(k);
            } else {
                entry.insert(k.clone(), v.clone());
            }
        }
        if entry.is_empty() {
            table.remove(edit.key);
            return Ok(None);
        }
        Ok(Some(entry.clone()))
    }
}

/// One change asked of `write_file` (issue #48).
pub struct Edit<'a> {
    /// The file to change.
    pub origin: Origin,
    pub scope: Scope,
    /// Matched exactly, like `join` does.
    pub key: &'a str,
    /// Top-level fields to set; a null value removes that field. Fields not
    /// named are kept. An entry left with no fields is removed.
    pub fields: &'a Map<String, Value>,
    /// Remove the whole entry. `fields` must then be empty.
    pub delete: bool,
}

impl Edit<'_> {
    /// Reject an edit that cannot be meant: one no component could ever
    /// match, or one that asks for nothing.
    pub fn check(&self) -> anyhow::Result<()> {
        let table = self.scope.table_name();
        if self.key.is_empty() {
            bail!("sidecar: the key is empty; an empty key never matches a component");
        }
        if self.scope == Scope::Refdes && self.origin == Origin::Library {
            bail!(
                "sidecar: the shared library (${LIBRARY_ENV}) cannot hold \"refdes\" entries; write them to the project file"
            );
        }
        if self.delete && !self.fields.is_empty() {
            bail!("sidecar: give either `fields` or `delete`, not both");
        }
        if !self.delete && self.fields.is_empty() {
            bail!("sidecar: nothing to write for {table}.{:?}: give `fields`, or `delete`", self.key);
        }
        Ok(())
    }
}

/// What `write_file` did.
#[derive(Debug, Clone, Serialize)]
pub struct Written {
    pub file: String,
    pub origin: Origin,
    pub scope: Scope,
    pub key: String,
    /// True when the project file did not exist and this write created it.
    pub created_file: bool,
    /// The entry as now stored in this file, or null when it was removed.
    /// Other files' entries for the same component are not merged in here;
    /// `get_component` shows the joined result.
    pub entry: Option<Map<String, Value>>,
}

/// Change one entry in the sidecar file at `path` (issue #48).
///
/// Holds an exclusive OS lock on `<file>.lock` for the whole
/// read-change-write, so two writers (in this process or another) take
/// turns. The file is re-read from disk under the lock, so entries another
/// writer or a person added since startup are kept. The new text goes to a
/// temp file in the same directory, which is renamed over the original. A
/// symlink is followed: the file it points at is replaced, not the link.
///
/// A missing project file is created; a missing library file is an error,
/// as at startup (#54). A symlink to a missing file is an error, so the link
/// is never replaced by a plain file. A file that does not parse is an error
/// and is left unchanged.
pub fn write_file(path: &Path, edit: &Edit) -> anyhow::Result<Written> {
    // Keys are stored trimmed; components are matched on trimmed keys too.
    let edit = &Edit { key: edit.key.trim(), ..*edit };
    edit.check()?;
    let shown = path.display().to_string();
    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(_) if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) => {
            bail!("sidecar {shown}: is a symlink to a file that does not exist; create the target first");
        }
        Err(_) if edit.origin == Origin::Library => return Err(missing_library(&shown, path)),
        Err(_) => path.to_path_buf(),
    };
    let mut lock_name = target.file_name().unwrap_or_default().to_os_string();
    lock_name.push(".lock");
    let lock_path = target.with_file_name(lock_name);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("sidecar {shown}: cannot open lock file {}", lock_path.display()))?;
    // Released when `lock` is dropped at the end of this function.
    lock.lock()
        .with_context(|| format!("sidecar {shown}: cannot lock {}", lock_path.display()))?;

    let (mut file, created_file) = match fs::read_to_string(&target) {
        Ok(text) => (SidecarFile::parse(&text, &shown, edit.origin)?, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && edit.origin == Origin::Project => {
            (SidecarFile::empty(&shown, edit.origin), true)
        }
        Err(e) => return Err(e).with_context(|| format!("sidecar {shown}: cannot read")),
    };
    let entry = file.apply(edit)?;

    let mut tmp_name = target.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = target.with_file_name(tmp_name);
    let result = (|| -> std::io::Result<()> {
        let mut out = fs::File::create(&tmp)?;
        // Keep the original file's permissions. Set before the data goes in,
        // so a private file is never readable through the temp file.
        if let Ok(meta) = fs::metadata(&target) {
            out.set_permissions(meta.permissions())?;
        }
        out.write_all(file.to_json().as_bytes())?;
        // On disk before the rename, so a crash cannot leave the sidecar
        // renamed but empty.
        out.sync_all()?;
        fs::rename(&tmp, &target)?;
        // Make the rename itself durable.
        #[cfg(unix)]
        if let Some(dir) = target.parent() {
            fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("sidecar {shown}: cannot write"));
    }
    Ok(Written {
        file: shown,
        origin: edit.origin,
        scope: edit.scope,
        key: edit.key.to_string(),
        created_file,
        entry,
    })
}

/// The error for a library path that names no file (#54), at startup or on
/// a write. Shows the absolute path tried, quoted, so a cwd mistake or a
/// blank value is visible in the message.
fn missing_library(shown: &str, path: &Path) -> anyhow::Error {
    let tried = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    anyhow::anyhow!(
        "sidecar {shown}: ${LIBRARY_ENV} names a file that does not exist (looked for \"{}\")",
        tried.display()
    )
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
    /// Where `load` looked: the project path is set even when that file
    /// was not found, so `write_sidecar` can create it. Both None for a
    /// sidecar that was not loaded from disk.
    project: Option<PathBuf>,
    library: Option<PathBuf>,
}

/// The identity a component is matched on.
pub struct Keys<'a> {
    pub refdes: &'a str,
    pub lib_id: &'a str,
    pub mpn: &'a str,
}

impl<'a> Keys<'a> {
    pub fn get(&self, scope: Scope) -> &'a str {
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
        Sidecar { files, probes, project: None, library: None }
    }

    /// The path of the file an `origin` write goes to.
    pub fn path_for(&self, origin: Origin) -> anyhow::Result<&Path> {
        match origin {
            Origin::Project => self.project.as_deref()
                .context("sidecar: no project file path (the sidecar was not loaded from disk)"),
            Origin::Library => self.library.as_deref().with_context(|| format!(
                "sidecar: no shared library is set; set ${LIBRARY_ENV} to a file and restart, or write to the project file"
            )),
        }
    }

    /// Load the same files again, from disk.
    pub fn reload(&self) -> anyhow::Result<Sidecar> {
        Self::load_paths(self.path_for(Origin::Project)?, self.library.as_deref())
    }

    /// The project file path for a netlist: same directory and stem,
    /// extension `sidecar.json` ("board.net" -> "board.sidecar.json").
    pub fn project_path(netlist: &Path) -> PathBuf {
        netlist.with_extension("sidecar.json")
    }

    /// Load the library (if `library` is given) and the project file next to
    /// `netlist`. A missing project file is skipped; a missing library is an
    /// error, since the user named it (#54). A file that exists but cannot be
    /// read or parsed is an error.
    pub fn load(netlist: &Path, library: Option<&Path>) -> anyhow::Result<Sidecar> {
        Self::load_paths(&Self::project_path(netlist), library)
    }

    /// `load`, given the project file path itself.
    fn load_paths(project: &Path, library: Option<&Path>) -> anyhow::Result<Sidecar> {
        let mut files = Vec::new();
        let mut probes = Vec::new();
        let candidates = library
            .map(|p| (p.to_path_buf(), Origin::Library))
            .into_iter()
            .chain(std::iter::once((project.to_path_buf(), Origin::Project)));
        for (path, origin) in candidates {
            let shown = path.display().to_string();
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    files.push(SidecarFile::parse(&text, &shown, origin)?);
                    probes.push(Probe { path: shown, origin, found: true });
                }
                // Issue #54: a library path the user set but that is missing
                // (a typo, or a relative path from another cwd) would turn
                // the library off without a word, so it stops startup.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && origin == Origin::Library => {
                    return Err(missing_library(&shown, &path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    probes.push(Probe { path: shown, origin, found: false });
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("sidecar {shown}: cannot read"));
                }
            }
        }
        Ok(Sidecar {
            files,
            probes,
            project: Some(project.to_path_buf()),
            library: library.map(Path::to_path_buf),
        })
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
    fn load_skips_missing_project_file() {
        let dir = std::env::temp_dir().join(format!("sidecar-test-{}", std::process::id()));
        let s = Sidecar::load(&dir.join("none.net"), None).expect("ok");
        assert!(s.files.is_empty());
        assert_eq!(s.probes.len(), 1);
        assert!(!s.probes[0].found);
        assert_eq!(s.probes[0].origin, Origin::Project);
    }

    // Issue #54: a library path that is set but missing stops startup, with
    // the path in the message.
    #[test]
    fn load_rejects_missing_library() {
        let dir = std::env::temp_dir().join(format!("sidecar-test-{}", std::process::id()));
        let lib = dir.join("nolib.json");
        let err = Sidecar::load(&dir.join("none.net"), Some(&lib)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&lib.display().to_string()), "{msg}");
        assert!(msg.contains("does not exist"), "{msg}");
        assert!(msg.contains(LIBRARY_ENV), "{msg}");
        let abs = std::path::absolute(&lib).unwrap();
        assert!(msg.contains(&format!("\"{}\"", abs.display())), "{msg}");
    }

    // --- write_file (issue #48) ---

    /// A fresh, empty directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sidecar-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fields(v: Value) -> Map<String, Value> {
        let Value::Object(m) = v else { panic!("fields must be an object") };
        m
    }

    fn edit<'a>(origin: Origin, scope: Scope, key: &'a str, f: &'a Map<String, Value>) -> Edit<'a> {
        Edit { origin, scope, key, fields: f, delete: false }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn write_creates_missing_project_file() {
        let dir = temp_dir("create");
        let path = dir.join("board.sidecar.json");
        let f = fields(json!({"note": "quad"}));
        let w = write_file(&path, &edit(Origin::Project, Scope::Mpn, "TLV9004", &f)).unwrap();
        assert!(w.created_file);
        assert_eq!(w.entry, Some(f.clone()));
        assert_eq!(read_json(&path), json!({"mpn": {"TLV9004": {"note": "quad"}}}));
        // The written file loads again.
        assert!(SidecarFile::parse(&fs::read_to_string(&path).unwrap(), "t", Origin::Project).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    // Fields merge per top-level key; null removes one; other entries and
    // tables are kept.
    #[test]
    fn write_merges_fields_and_null_removes() {
        let dir = temp_dir("merge");
        let path = dir.join("board.sidecar.json");
        fs::write(&path, json!({
            "lib_id": {"Amp:Quad": {"x": 1}},
            "mpn": {"TLV9004": {"a": 1, "b": 2}, "OTHER": {"c": 3}}
        }).to_string()).unwrap();
        let f = fields(json!({"b": null, "d": {"pins": [1, 2]}}));
        let w = write_file(&path, &edit(Origin::Project, Scope::Mpn, "TLV9004", &f)).unwrap();
        assert!(!w.created_file);
        assert_eq!(Value::Object(w.entry.unwrap()), json!({"a": 1, "d": {"pins": [1, 2]}}));
        assert_eq!(read_json(&path), json!({
            "lib_id": {"Amp:Quad": {"x": 1}},
            "mpn": {"TLV9004": {"a": 1, "d": {"pins": [1, 2]}}, "OTHER": {"c": 3}}
        }));
        fs::remove_dir_all(&dir).unwrap();
    }

    // Nulling the last field removes the entry; the emptied table is left
    // out of the file.
    #[test]
    fn write_removes_entry_left_empty() {
        let dir = temp_dir("empty");
        let path = dir.join("board.sidecar.json");
        fs::write(&path, r#"{"refdes": {"U1": {"a": 1}}, "mpn": {"X": {"n": 1}}}"#).unwrap();
        let f = fields(json!({"a": null}));
        let w = write_file(&path, &edit(Origin::Project, Scope::Refdes, "U1", &f)).unwrap();
        assert_eq!(w.entry, None);
        assert_eq!(read_json(&path), json!({"mpn": {"X": {"n": 1}}}));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_delete_removes_entry_and_missing_is_an_error() {
        let dir = temp_dir("delete");
        let path = dir.join("board.sidecar.json");
        fs::write(&path, r#"{"mpn": {"X": {"n": 1}, "Y": {"n": 2}}}"#).unwrap();
        let none = Map::new();
        let del = |key| Edit { origin: Origin::Project, scope: Scope::Mpn, key, fields: &none, delete: true };
        assert_eq!(write_file(&path, &del("X")).unwrap().entry, None);
        assert_eq!(read_json(&path), json!({"mpn": {"Y": {"n": 2}}}));
        let err = write_file(&path, &del("X")).unwrap_err();
        assert!(format!("{err:#}").contains("no mpn.\"X\" entry to delete"), "{err:#}");
        fs::remove_dir_all(&dir).unwrap();
    }

    // The file is re-read under the lock: an entry a person (or another
    // server) added after this server loaded it survives the write.
    #[test]
    fn write_keeps_entries_added_on_disk_since_load() {
        let dir = temp_dir("reread");
        let net = dir.join("board.net");
        let path = Sidecar::project_path(&net);
        fs::write(&path, r#"{"mpn": {"X": {"n": 1}}}"#).unwrap();
        let loaded = Sidecar::load(&net, None).unwrap();
        fs::write(&path, r#"{"mpn": {"X": {"n": 1}, "BY-HAND": {"n": 2}}}"#).unwrap();
        let f = fields(json!({"n": 3}));
        let target = loaded.path_for(Origin::Project).unwrap();
        write_file(target, &edit(Origin::Project, Scope::Mpn, "X", &f)).unwrap();
        assert_eq!(read_json(&path), json!({"mpn": {"X": {"n": 3}, "BY-HAND": {"n": 2}}}));
        let fresh = loaded.reload().unwrap();
        assert_eq!(fresh.join(&keys("U1", "", "BY-HAND")).unwrap().entry["n"], json!(2));
        fs::remove_dir_all(&dir).unwrap();
    }

    // Many writers on one file take turns under the lock: no entry is lost.
    #[test]
    fn concurrent_writes_all_land() {
        let dir = temp_dir("concurrent");
        let path = dir.join("lib.json");
        fs::write(&path, "{}").unwrap();
        let handles: Vec<_> = (0..16).map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                let f = fields(json!({"i": i}));
                let key = format!("P{i}");
                write_file(&path, &edit(Origin::Library, Scope::Mpn, &key, &f)).unwrap();
            })
        }).collect();
        for h in handles {
            h.join().unwrap();
        }
        let v = read_json(&path);
        assert_eq!(v["mpn"].as_object().unwrap().len(), 16, "{v}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_rejects_impossible_edits() {
        let dir = temp_dir("reject");
        let path = dir.join("board.sidecar.json");
        let f = fields(json!({"n": 1}));
        let none = Map::new();
        let nulls = fields(json!({"n": null}));
        let cases: [(Edit, &str); 5] = [
            (edit(Origin::Library, Scope::Refdes, "U1", &f), "cannot hold \"refdes\""),
            (edit(Origin::Project, Scope::Mpn, "", &f), "the key is empty"),
            (edit(Origin::Project, Scope::Mpn, "X", &none), "nothing to write"),
            // Only nulls, on an entry that does not exist: nothing to remove,
            // most likely a typo in the key. Same as `delete` on a missing one.
            (edit(Origin::Project, Scope::Mpn, "X", &nulls), "no mpn.\"X\" entry to remove fields from"),
            (Edit { origin: Origin::Project, scope: Scope::Mpn, key: "X", fields: &f, delete: true },
             "not both"),
        ];
        for (e, want) in cases {
            let err = write_file(&path, &e).unwrap_err();
            assert!(format!("{err:#}").contains(want), "{want}: {err:#}");
        }
        assert!(!path.exists(), "a rejected edit must not create the file");
        fs::remove_dir_all(&dir).unwrap();
    }

    // A missing library is not created (as at startup, #54), and a file
    // that does not parse is left as it was.
    #[test]
    fn write_refuses_missing_library_and_malformed_file() {
        let dir = temp_dir("refuse");
        let f = fields(json!({"n": 1}));
        let lib = dir.join("lib.json");
        let err = write_file(&lib, &edit(Origin::Library, Scope::Mpn, "X", &f)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(LIBRARY_ENV) && msg.contains("does not exist"), "{msg}");
        assert!(!lib.exists());
        assert!(!dir.join("lib.json.lock").exists(), "no stray lock file");
        let path = dir.join("board.sidecar.json");
        fs::write(&path, r#"{"mpns": {}}"#).unwrap();
        let err = write_file(&path, &edit(Origin::Project, Scope::Mpn, "X", &f)).unwrap_err();
        assert!(format!("{err:#}").contains("unknown top-level key"), "{err:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"mpns": {}}"#);
        fs::remove_dir_all(&dir).unwrap();
    }

    // A symlinked sidecar stays a symlink; the file it points at changes.
    #[cfg(unix)]
    #[test]
    fn write_follows_symlink() {
        let dir = temp_dir("symlink");
        let real = dir.join("real.json");
        fs::write(&real, "{}").unwrap();
        let link = dir.join("lib.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let f = fields(json!({"n": 1}));
        write_file(&link, &edit(Origin::Library, Scope::Mpn, "X", &f)).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(read_json(&real), json!({"mpn": {"X": {"n": 1}}}));
        fs::remove_dir_all(&dir).unwrap();
    }

    // A symlink whose target is missing is refused, not replaced by a plain
    // file (which would cut the link to the shared target without a word).
    #[cfg(unix)]
    #[test]
    fn write_refuses_dangling_symlink() {
        let dir = temp_dir("dangling");
        let link = dir.join("board.sidecar.json");
        std::os::unix::fs::symlink(dir.join("missing.json"), &link).unwrap();
        let f = fields(json!({"n": 1}));
        let err = write_file(&link, &edit(Origin::Project, Scope::Mpn, "X", &f)).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(!dir.join("missing.json").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    // Keys are stored trimmed (components are matched on trimmed keys); a
    // key of only spaces is empty.
    #[test]
    fn write_trims_key() {
        let dir = temp_dir("space");
        let path = dir.join("board.sidecar.json");
        let f = fields(json!({"n": 1}));
        let w = write_file(&path, &edit(Origin::Project, Scope::Mpn, " X ", &f)).unwrap();
        assert_eq!(w.key, "X");
        assert_eq!(read_json(&path), json!({"mpn": {"X": {"n": 1}}}));
        let err = write_file(&path, &edit(Origin::Project, Scope::Mpn, "  ", &f)).unwrap_err();
        assert!(format!("{err:#}").contains("the key is empty"), "{err:#}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_for_library_needs_the_env_var() {
        let s = Sidecar::default();
        let err = s.path_for(Origin::Library).unwrap_err();
        assert!(format!("{err:#}").contains(LIBRARY_ENV), "{err:#}");
        assert!(s.path_for(Origin::Project).is_err());
    }

    // join() precedence needs library files before project files; load()
    // must probe in that order.
    #[test]
    fn load_reads_library_before_project() {
        let dir = std::env::temp_dir().join(format!("sidecar-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("lib.json");
        std::fs::write(&lib, r#"{"mpn": {"X": {"n": 1}}}"#).unwrap();
        std::fs::write(dir.join("board.sidecar.json"), r#"{"mpn": {"X": {"n": 2}}}"#).unwrap();
        let s = Sidecar::load(&dir.join("board.net"), Some(&lib));
        std::fs::remove_dir_all(&dir).unwrap();
        let s = s.expect("ok");
        let order: Vec<(Origin, bool)> = s.probes.iter().map(|p| (p.origin, p.found)).collect();
        assert_eq!(order, vec![(Origin::Library, true), (Origin::Project, true)]);
        let j = s.join(&keys("U1", "", "X")).expect("matches");
        assert_eq!(j.entry["n"], json!(2));
    }
}
