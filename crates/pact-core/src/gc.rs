//! `pact gc` (issue #351): the state directory grew without bound. After
//! about ten benchmark runs it held 100 MB of lane and planner logs, ACP
//! lean homes and sidecar records, none of which `teardown` removes
//! because `inspect` still reads them while the workspace exists.
//!
//! Rules: anything named after a workspace (its log, home and sidecar
//! records) goes once that workspace is gone and the item is older than
//! the cutoff; anything shared by a run (planner and arbiter logs, ACP
//! homes, briefs, verification logs, scratch) goes once it is older than
//! the cutoff. Age is the newest modification anywhere inside an item, so
//! an ACP home still being written is never old. Plans are kept: they are
//! the artefact a user edits and reruns.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::Orchestrator;

/// One file or directory `gc` removed, or would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcItem {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Directories holding one item per workspace id, and the item's suffix.
const PER_WORKSPACE: &[(&str, &str)] = &[
    ("logs", ".jsonl"),
    ("homes", ""),
    ("mcp", ".json"),
    ("meta/runs", ".json"),
    ("meta/deps", ".json"),
    ("meta/prepare", ".json"),
    ("meta/replies", ".json"),
];

/// Directories whose every entry belongs to a run, not a workspace.
const PER_RUN: &[&str] = &["briefs", "meta/verify", "integration", "tmp"];

/// Size in bytes and newest modification time of everything under `path`.
fn measure(path: &Path) -> (u64, SystemTime) {
    let mut bytes = 0;
    let mut newest = SystemTime::UNIX_EPOCH;
    for entry in walkdir::WalkDir::new(path).into_iter().filter_map(Result::ok) {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                bytes += meta.len();
            }
            if let Ok(modified) = meta.modified() {
                newest = newest.max(modified);
            }
        }
    }
    (bytes, newest)
}

/// The workspace id a per-workspace entry is named for, if it is one:
/// shared entries in the same directories (planner and arbiter logs, ACP
/// and planner homes) never are.
fn workspace_id_of(name: &str, suffix: &str) -> Option<String> {
    if ["planner-", "arbiter-", "acp-"].iter().any(|prefix| name.starts_with(prefix)) {
        return None;
    }
    if name.ends_with(".cancel") {
        return Some(name.trim_end_matches(".cancel").to_string());
    }
    name.strip_suffix(suffix).filter(|id| !id.is_empty()).map(str::to_string)
}

/// Everything in `state_dir` that `gc` would remove, given the ids of the
/// workspaces that still exist and the cutoff time.
pub fn collect(state_dir: &Path, live: &[String], cutoff: SystemTime) -> Vec<GcItem> {
    let mut items = Vec::new();
    let consider = |path: PathBuf, items: &mut Vec<GcItem>| {
        let (bytes, newest) = measure(&path);
        if newest < cutoff {
            items.push(GcItem { path, bytes });
        }
    };
    let entries = |dir: &Path| -> Vec<(String, PathBuf)> {
        std::fs::read_dir(dir)
            .map(|read| read.filter_map(Result::ok).map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect())
            .unwrap_or_default()
    };
    for (dir, suffix) in PER_WORKSPACE {
        for (name, path) in entries(&state_dir.join(dir)) {
            match workspace_id_of(&name, suffix) {
                Some(id) if live.contains(&id) => {}
                _ => consider(path, &mut items),
            }
        }
    }
    for (name, path) in entries(&state_dir.join("meta")) {
        if let Some(id) = name.strip_suffix(".cancel") {
            if !live.iter().any(|live| live == id) {
                consider(path, &mut items);
            }
        }
    }
    for dir in PER_RUN {
        for (_, path) in entries(&state_dir.join(dir)) {
            consider(path, &mut items);
        }
    }
    items.sort_by(|a, b| a.path.cmp(&b.path));
    items
}

impl Orchestrator {
    /// Removes (or with `dry_run` only lists) state older than `older_than`
    /// that no existing workspace still uses.
    pub fn gc(&self, older_than: Duration, dry_run: bool) -> Result<Vec<GcItem>> {
        let live: Vec<String> = self.workspaces.list_workspaces()?.into_iter().map(|w| w.id).collect();
        let cutoff = SystemTime::now().checked_sub(older_than).unwrap_or(SystemTime::UNIX_EPOCH);
        let items = collect(self.workspaces.state_dir(), &live, cutoff);
        if !dry_run {
            for item in &items {
                let removed = if item.path.is_dir() { std::fs::remove_dir_all(&item.path) } else { std::fs::remove_file(&item.path) };
                if let Err(err) = removed {
                    tracing::warn!("gc: could not remove {}: {err}", item.path.display());
                }
            }
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn dead_workspaces_and_old_run_state_go_live_ones_and_plans_stay() {
        let root = std::env::temp_dir().join(format!("pact-gc-{}", uuid::Uuid::new_v4()));
        let keep = [
            "logs/live.jsonl",
            "homes/live/settings.json",
            "meta/runs/live.json",
            "meta/live.cancel",
            "meta/plans/1-task.json",
            "meta/live.json",
        ];
        let remove = [
            "logs/gone.jsonl",
            "logs/planner-1.jsonl",
            "logs/arbiter-x.decision.json",
            "homes/gone/settings.json",
            "homes/acp-copilot-abcd/session-state/e.jsonl",
            "meta/runs/gone.json",
            "meta/deps/gone.json",
            "meta/gone.cancel",
            "briefs/1/alpha.md",
            "meta/verify/1-ab/1-npm-test-after.log",
        ];
        for file in keep.iter().chain(&remove) {
            touch(&root.join(file));
        }
        let live = vec!["live".to_string()];

        assert!(collect(&root, &live, SystemTime::UNIX_EPOCH).is_empty(), "nothing is older than the epoch");

        let future = SystemTime::now() + Duration::from_secs(60);
        let found: Vec<String> = collect(&root, &live, future)
            .iter()
            .map(|item| item.path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect();
        let expected = ["briefs/1", "homes/acp-copilot-abcd", "homes/gone", "logs/arbiter-x.decision.json", "logs/gone.jsonl", "logs/planner-1.jsonl", "meta/deps/gone.json", "meta/gone.cancel", "meta/runs/gone.json", "meta/verify/1-ab"];
        assert_eq!(found, expected);
        let _ = std::fs::remove_dir_all(&root);
    }
}
