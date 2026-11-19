//! File-ownership enforcement for `pact run` (issue #409). The plan is
//! disjoint by validation; this checks what the lanes actually wrote. A
//! changed file no unit owns is caught from the tree itself. A write into
//! another unit's file is attributed from the lane's ACP log, whose
//! `tool_call` updates name the paths an edit touched, and, for lanes
//! without such a log, from each unit's files hashed when its lane
//! finished: a file that changed after its owner finished was written by
//! a lane still running then.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::Hasher;
use std::path::Path;
use std::sync::Mutex;

use crate::run::Plan;

/// One file a lane wrote that its unit does not own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipViolation {
    /// Repo-relative, `/`-separated.
    pub file: String,
    /// The units whose lanes may have written it: one when the log names
    /// the lane, several when only the timing narrows it down, none when
    /// nothing does.
    pub writers: Vec<String>,
    /// The unit that owns the file; `None` when no unit does.
    pub owner: Option<String>,
}

impl std::fmt::Display for OwnershipViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let file = &self.file;
        match (self.writers.as_slice(), &self.owner) {
            ([writer], Some(owner)) => write!(f, "lane `{writer}` wrote `{file}`, which unit `{owner}` owns"),
            ([writer], None) => write!(f, "lane `{writer}` wrote `{file}`, which no unit owns"),
            ([], Some(owner)) => write!(f, "`{file}`, which unit `{owner}` owns, changed after its lane finished"),
            ([], None) => write!(f, "`{file}` changed, and no unit owns it"),
            (writers, Some(owner)) => write!(
                f,
                "`{file}`, which unit `{owner}` owns, changed after its lane finished, while {} were still running",
                writers.iter().map(|w| format!("`{w}`")).collect::<Vec<_>>().join(", ")
            ),
            (writers, None) => write!(f, "`{file}` changed, no unit owns it, and one of {} wrote it", writers.join(", ")),
        }
    }
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    hasher.write(&bytes);
    Some(hasher.finish())
}

type UnitSnapshot = (usize, Vec<(String, Option<u64>)>);

/// Each unit's owned files, hashed the moment its lane finished, in the
/// order the lanes finished.
#[derive(Default)]
pub(crate) struct FinishSnapshots {
    finished: Mutex<Vec<UnitSnapshot>>,
}

impl FinishSnapshots {
    pub(crate) fn record(&self, unit: usize, files: &[String], tree: &Path) {
        let snapshot = files.iter().map(|file| (normalize(file), hash_file(&tree.join(normalize(file))))).collect();
        self.finished.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((unit, snapshot));
    }

    fn take(self) -> Vec<UnitSnapshot> {
        self.finished.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The tree-relative paths an ACP lane's log says it edited, deleted or
/// moved. Reads and searches name locations too, so only those kinds
/// count. A log in another shape (a process lane's) yields nothing.
pub(crate) fn edits_in_log(log: &Path, tree: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(log) else { return Vec::new() };
    let tree_prefix = normalize(&tree.to_string_lossy()).trim_end_matches('/').to_ascii_lowercase() + "/";
    let mut kinds: HashMap<String, String> = HashMap::new();
    let mut edits: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(update) = value.get("update") else { continue };
        if !matches!(update.get("sessionUpdate").and_then(|v| v.as_str()), Some("tool_call" | "tool_call_update")) {
            continue;
        }
        let call = update.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(kind) = update.get("kind").and_then(|v| v.as_str()) {
            kinds.insert(call.clone(), kind.to_string());
        }
        if !matches!(kinds.get(&call).map(String::as_str), Some("edit" | "delete" | "move")) {
            continue;
        }
        for location in update.get("locations").and_then(|v| v.as_array()).into_iter().flatten() {
            let Some(path) = location.get("path").and_then(|v| v.as_str()) else { continue };
            let path = normalize(path);
            let relative = if path.to_ascii_lowercase().starts_with(&tree_prefix) {
                path[tree_prefix.len()..].to_string()
            } else if path.starts_with('/') || path.chars().nth(1) == Some(':') {
                continue;
            } else {
                path
            };
            if !edits.contains(&relative) {
                edits.push(relative);
            }
        }
    }
    edits
}

/// Every ownership violation in a finished batch: `changed` is what the
/// tree carries over its base, `lanes` the units whose lanes ran (one
/// wave's, issue #462), `lane_edits` the paths each unit's lane logged
/// editing, `snapshots` the owned files hashed as each lane finished and
/// `current_hash` the same hash now.
pub(crate) fn check_ownership(
    plan: &Plan,
    lanes: &[usize],
    changed: &[String],
    lane_edits: &[(usize, Vec<String>)],
    snapshots: FinishSnapshots,
    current_hash: impl Fn(&str) -> Option<u64>,
) -> Vec<OwnershipViolation> {
    let name = |unit: usize| plan.units[unit].name.trim().to_string();
    let owner_of = |path: &str| plan.units.iter().position(|unit| unit.files.iter().any(|file| normalize(file) == path));
    let mut violations: Vec<OwnershipViolation> = Vec::new();
    let add = |violations: &mut Vec<OwnershipViolation>, file: &str, writers: Vec<String>, owner: Option<usize>| {
        if !violations.iter().any(|v| v.file == file) {
            violations.push(OwnershipViolation { file: file.to_string(), writers, owner: owner.map(name) });
        }
    };

    for (unit, edits) in lane_edits {
        for file in edits {
            match owner_of(file) {
                Some(owner) if owner == *unit => {}
                Some(owner) => add(&mut violations, file, vec![name(*unit)], Some(owner)),
                None if changed.contains(file) => add(&mut violations, file, vec![name(*unit)], None),
                None => {}
            }
        }
    }
    for file in changed {
        if owner_of(file).is_none() {
            add(&mut violations, file, Vec::new(), None);
        }
    }
    let finished = snapshots.take();
    for (position, (unit, files)) in finished.iter().enumerate() {
        for (file, at_finish) in files {
            if current_hash(file) == *at_finish {
                continue;
            }
            let still_running: Vec<String> = lanes
                .iter()
                .copied()
                .filter(|other| other != unit && !finished[..=position].iter().any(|(done, _)| done == other))
                .map(name)
                .collect();
            if !still_running.is_empty() {
                add(&mut violations, file, still_running, Some(*unit));
            }
        }
    }
    violations.sort_by(|a, b| a.file.cmp(&b.file));
    violations
}

/// `check_ownership`'s `current_hash` for a real tree.
pub(crate) fn tree_hash(tree: &Path) -> impl Fn(&str) -> Option<u64> + '_ {
    move |file| hash_file(&tree.join(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::PlanUnit;

    fn plan(units: &[(&str, &[&str])]) -> Plan {
        Plan {
            task: "t".into(),
            shared_context: String::new(),
            units: units.iter().map(|(name, files)| PlanUnit { name: name.to_string(), files: files.iter().map(|f| f.to_string()).collect(), brief: "b".into(), verify: None, depends_on: vec![] }).collect(),
            verify: vec![],
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_logged_write_into_another_units_file_or_an_unowned_file_names_the_lane() {
        let p = plan(&[("alpha", &["a.txt"]), ("beta", &["b.txt"])]);
        let changed = strings(&["a.txt", "b.txt", "shared.json", "stray.txt"]);
        let edits = vec![(0, strings(&["a.txt", "b.txt", "shared.json"])), (1, strings(&["b.txt"]))];
        let found = check_ownership(&p, &[0, 1], &changed, &edits, FinishSnapshots::default(), |_| None);
        let lines: Vec<String> = found.iter().map(ToString::to_string).collect();
        assert_eq!(
            lines,
            [
                "lane `alpha` wrote `b.txt`, which unit `beta` owns",
                "lane `alpha` wrote `shared.json`, which no unit owns",
                "`stray.txt` changed, and no unit owns it",
            ]
        );
    }

    #[test]
    fn a_file_changed_after_its_owner_finished_is_blamed_on_the_lanes_still_running() {
        let p = plan(&[("alpha", &["a.txt"]), ("beta", &["b.txt"]), ("gamma", &["c.txt"])]);
        let snapshots = FinishSnapshots::default();
        snapshots.finished.lock().unwrap().extend([(1, vec![("b.txt".to_string(), Some(1))]), (2, vec![("c.txt".to_string(), Some(7))])]);
        let now = |file: &str| Some(if file == "b.txt" { 2 } else { 7 });
        let found = check_ownership(&p, &[0, 1, 2], &strings(&["a.txt", "b.txt", "c.txt"]), &[], snapshots, now);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].owner.as_deref(), Some("beta"));
        assert_eq!(found[0].writers, ["alpha", "gamma"], "gamma finished after beta and alpha never reported finishing");
    }

    #[test]
    fn only_edits_inside_the_tree_count_and_reads_do_not() {
        let tree = std::env::temp_dir().join(format!("pact-ownership-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tree).unwrap();
        let abs = |file: &str| tree.join(file).display().to_string();
        let lines = [
            serde_json::json!({ "update": { "sessionUpdate": "tool_call", "toolCallId": "1", "kind": "read", "locations": [{ "path": abs("read.txt") }] } }),
            serde_json::json!({ "update": { "sessionUpdate": "tool_call", "toolCallId": "2", "kind": "edit", "locations": [] } }),
            serde_json::json!({ "update": { "sessionUpdate": "tool_call_update", "toolCallId": "2", "locations": [{ "path": abs("src/a.ts") }] } }),
            serde_json::json!({ "update": { "sessionUpdate": "tool_call", "toolCallId": "3", "kind": "delete", "locations": [{ "path": "old.ts" }] } }),
            serde_json::json!({ "update": { "sessionUpdate": "tool_call", "toolCallId": "4", "kind": "edit", "locations": [{ "path": "/elsewhere/x.ts" }] } }),
        ];
        let log = tree.join("lane.jsonl");
        std::fs::write(&log, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n") + "\nnot json\n").unwrap();
        assert_eq!(edits_in_log(&log, &tree), ["src/a.ts", "old.ts"]);
        let _ = std::fs::remove_dir_all(&tree);
    }
}
