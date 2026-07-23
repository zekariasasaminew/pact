mod lock;

pub use lock::PidLock;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Workspace {
    pub id: String,
    pub path: PathBuf,
    pub branch: String,
    pub task: String,
    pub created_at: u64,
    /// PID of the agent process currently running in this workspace, if
    /// any. Set right after the process is spawned (before blocking on its
    /// output) so a `teardown` invoked from a *different* CLI call -- while
    /// the agent is still running in another terminal -- can find and kill
    /// it before removing the worktree out from under it.
    #[serde(default)]
    pub agent_pid: Option<u32>,
    /// The repo root's `HEAD` at the moment this workspace's worktree was
    /// created -- i.e. the commit its branch actually forked from. Recorded
    /// so `merge_all`'s moving-base check has a real value to compare
    /// against later, rather than recomputing `merge-base` against
    /// whatever HEAD happens to be *at merge time* (which can't tell
    /// "moved forward normally" apart from "no longer part of this
    /// branch's history at all"). `#[serde(default)]` so workspace metadata
    /// persisted before this field existed still deserializes -- callers
    /// treat an empty string as "unknown, can't check".
    #[serde(default)]
    pub base_commit: String,
}

/// What an agent has actually done in one workspace, split into the
/// committed side (on its branch, relative to the repo's merge-base) and
/// the uncommitted side (still only in its working tree) -- see
/// `WorkspaceManager::workspace_diff`.
#[derive(Debug, Clone)]
pub struct WorkspaceDiff {
    /// `git log --oneline <merge-base>..<branch>`, empty if no merge-base
    /// could be found (e.g. an unrelated history).
    pub commit_log: String,
    /// `git diff --stat <merge-base>..<branch>`.
    pub committed_summary: String,
    /// `git status --porcelain` -- empty means a clean working tree.
    pub uncommitted_status: String,
    /// `git diff --stat HEAD` -- staged and unstaged changes together.
    pub uncommitted_summary: String,
}

/// A workspace's merge-base and the flat set of files it has touched since
/// -- see `WorkspaceManager::workspace_changes`.
#[derive(Debug, Clone)]
pub struct WorkspaceChanges {
    /// Empty if no merge-base could be found (e.g. unrelated history) --
    /// callers should treat that as "not comparable" rather than as a
    /// merge-base of the empty string.
    pub merge_base: String,
    /// Forward-slash-normalized relative paths, deduplicated and sorted.
    pub files: Vec<String>,
}

/// One workspace whose branch was merged cleanly into the integration
/// branch during `merge_all`.
#[derive(Debug, Clone)]
pub struct MergedWorkspace {
    pub id: String,
    pub branch: String,
    /// Files that had a real merge conflict but were resolved automatically
    /// (package.json dependency-block merge, or a `--union` match) rather
    /// than by git's plain 3-way merge -- surfaced explicitly rather than
    /// folded silently into "merged", since auto-resolution is exactly the
    /// kind of thing a user should be able to double-check. Empty for a
    /// workspace that merged with no conflicts at all.
    pub auto_resolved: Vec<String>,
    /// Files resolved by the Arbiter fallback (an agent's proposed
    /// resolution, accepted only after the caller's test command passed in
    /// the same worktree) -- kept separate from `auto_resolved` since this
    /// carries a meaningfully different trust level: a verified AI-proposed
    /// resolution, not a deterministic rule. Always empty unless Arbiter
    /// was configured for this run.
    pub arbiter_resolved: Vec<String>,
}

/// One workspace `merge_all` left out, and why -- either a real merge
/// conflict, or the moving-base check refusing it. Never blocks the rest of
/// the batch; see `MergeReport`.
#[derive(Debug, Clone)]
pub struct SkippedWorkspace {
    pub id: String,
    pub branch: String,
    pub reason: String,
}

/// A `skipped` workspace whose skip was specifically a real merge
/// conflict (never the moving-base check) -- the structured subset of
/// `SkippedWorkspace` that's actually resumable via
/// `WorkspaceManager::resolve_conflict`, since a moving-base skip needs a
/// rebased/recreated workspace, not a retried merge. See DESIGN.md
/// ("pact-vcs > Persisted conflicts (issue #85)").
#[derive(Debug, Clone)]
pub struct ConflictedWorkspace {
    pub id: String,
    pub branch: String,
    pub target_branch: String,
    pub files: Vec<String>,
}

/// The result of one `WorkspaceManager::merge_all` run -- see that method's
/// doc comment for the phases that produce this.
#[derive(Debug, Clone)]
pub struct MergeReport {
    pub target_branch: String,
    /// The repo HEAD every merged/skipped/planned workspace was compared
    /// against.
    pub base_commit: String,
    /// Populated only for a real run (`dry_run: false`): workspaces whose
    /// branch actually got merged into `target_branch`, in the order they
    /// were merged.
    pub merged: Vec<MergedWorkspace>,
    /// Workspaces left out either by the moving-base check (real and
    /// dry runs both) or by a genuine merge conflict (real runs only).
    pub skipped: Vec<SkippedWorkspace>,
    /// The subset of `skipped` that were a real merge conflict, in the
    /// structured shape `resolve_conflict` needs -- always empty for
    /// `--dry-run` (nothing is actually attempted, so nothing can conflict)
    /// and for a moving-base-only skip.
    pub conflicted: Vec<ConflictedWorkspace>,
    /// Populated only for `--dry-run`: the merge order that *would* be
    /// used, after sequencing and the moving-base check, without any git
    /// state actually being touched.
    pub planned: Vec<String>,
    pub dry_run: bool,
}

enum MergeOutcome {
    Merged {
        auto_resolved: Vec<String>,
        arbiter_resolved: Vec<String>,
    },
    Conflict {
        files: Vec<String>,
    },
}

/// The result of one `WorkspaceManager::resolve_conflict` attempt.
#[derive(Debug, Clone)]
pub enum ResolveOutcome {
    Resolved {
        auto_resolved: Vec<String>,
        arbiter_resolved: Vec<String>,
    },
    StillConflicted {
        files: Vec<String>,
    },
}

/// Given `(worktree_path, the workspace's task text, the still-unresolved
/// conflicted files)`, returns exactly the subset it resolved and staged
/// (`git add`) itself -- see DESIGN.md ("pact-vcs > Arbiter resolver
/// hook").
pub type ArbiterResolver<'a> = dyn Fn(&Path, &str, &[String]) -> Vec<String> + 'a;

const PACKAGE_JSON_DEP_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

/// Never auto-resolved, even under `--union` -- see DESIGN.md ("pact-vcs >
/// Semantic auto-resolution").
const NEVER_AUTO_RESOLVE: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "Gemfile.lock",
    "poetry.lock",
    "composer.lock",
    "Pipfile.lock",
    "go.sum",
];

/// Owns the lifecycle of git-worktree-backed agent workspaces for one repo.
/// State (locks, worktree metadata, and the worktrees themselves) lives as a
/// sibling of the repo, not inside its working tree, so it never shows up in
/// `git status` for the main repo.
pub struct WorkspaceManager {
    repo_root: PathBuf,
    state_dir: PathBuf,
}

impl WorkspaceManager {
    pub fn open(repo_root: impl Into<PathBuf>) -> Result<Self> {
        let repo_root = repo_root.into();
        if !repo_root.join(".git").exists() {
            bail!(
                "{} does not look like a git repository root (no .git found)",
                repo_root.display()
            );
        }

        let repo_name = repo_root
            .file_name()
            .context("repo root has no directory name")?;
        let state_dir = repo_root
            .parent()
            .context("repo root has no parent directory")?
            .join(format!(".pact-{}", repo_name.to_string_lossy()));

        std::fs::create_dir_all(state_dir.join("locks"))?;
        std::fs::create_dir_all(state_dir.join("meta"))?;
        std::fs::create_dir_all(state_dir.join("workspaces"))?;

        Ok(Self {
            repo_root,
            state_dir,
        })
    }

    pub fn state_dir(&self) -> &PathBuf {
        &self.state_dir
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir.join("locks").join("git.lock")
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.state_dir.join("meta").join(format!("{id}.json"))
    }

    /// Computes the id/branch/path a call to `create_workspace` would use,
    /// without touching git or disk -- lets a caller preview what a real
    /// spawn would create (see issue #16's `--dry-run`) using the exact
    /// same id-generation path `create_workspace` itself uses.
    pub fn preview_workspace_location(&self) -> (String, String, PathBuf) {
        let id = short_id();
        let branch = format!("pact/{id}");
        let path = self.state_dir.join("workspaces").join(&id);
        (id, branch, path)
    }

    pub fn create_workspace(&self, task: &str) -> Result<Workspace> {
        let (id, branch, path) = self.preview_workspace_location();

        let base_commit = {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;

            let base_commit = run_git_text(&self.repo_root, &["rev-parse", "HEAD"])?;

            let output = Command::new("git")
                .args(["worktree", "add"])
                .arg(&path)
                .args(["-b", &branch])
                .current_dir(&self.repo_root)
                .output()
                .context("failed to spawn `git worktree add`")?;

            if !output.status.success() {
                bail!(
                    "git worktree add failed:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            base_commit
        }; // lock released here

        let workspace = Workspace {
            id: id.clone(),
            path,
            branch,
            task: task.to_string(),
            created_at: now_unix(),
            agent_pid: None,
            base_commit,
        };

        std::fs::write(self.meta_path(&id), serde_json::to_vec_pretty(&workspace)?)
            .context("writing workspace metadata")?;

        Ok(workspace)
    }

    /// Records (or clears, with `None`) the PID of the agent process running
    /// in workspace `id`. Best-effort: a failure to persist this shouldn't
    /// abort whatever launched the agent, so callers typically log rather
    /// than propagate an error from this.
    pub fn set_agent_pid(&self, id: &str, pid: Option<u32>) -> Result<()> {
        let mut workspace = self.get_workspace(id)?;
        workspace.agent_pid = pid;
        std::fs::write(self.meta_path(id), serde_json::to_vec_pretty(&workspace)?)
            .context("writing workspace metadata")?;
        Ok(())
    }

    /// Removes a workspace's worktree and, unless `keep_branch` is set,
    /// the `pact/<id>` branch created for it. Refuses on uncommitted
    /// changes unless `force` is set -- see DESIGN.md ("pact-vcs >
    /// Workspace teardown").
    pub fn remove_workspace(&self, id: &str, keep_branch: bool, force: bool) -> Result<()> {
        let workspace = self.get_workspace(id)?;

        if !force {
            let dirty = self.dirty_status(&workspace.path)?;
            if !dirty.is_empty() {
                bail!(
                    "workspace {id} has uncommitted changes -- refusing to tear it down \
                     (would silently discard them). Run `pact diff {id}` to inspect, or pass \
                     --force to discard them anyway:\n{dirty}"
                );
            }
        }

        kill_if_alive(&workspace);

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;
            self.remove_worktree_retrying(&workspace.path)?;
            if !keep_branch {
                self.delete_branch(&workspace.branch);
            }
        }

        let _ = std::fs::remove_file(self.meta_path(id));
        Ok(())
    }

    /// Raw `git status --porcelain` output for a workspace -- empty means
    /// clean. Used both to gate `remove_workspace` and to show `list` a
    /// quick per-workspace dirty/clean indicator without needing a full
    /// `diff` call for every workspace just to check.
    fn dirty_status(&self, path: &std::path::Path) -> Result<String> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(path)
            .output()
            .context("failed to spawn `git status`")?;
        if !output.status.success() {
            bail!(
                "git status failed in {}:\n{}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
    }

    /// Whether a workspace has any uncommitted changes (staged, unstaged,
    /// or untracked). Cheap enough to call once per workspace in `list`.
    pub fn is_dirty(&self, id: &str) -> Result<bool> {
        let workspace = self.get_workspace(id)?;
        Ok(!self.dirty_status(&workspace.path)?.is_empty())
    }

    /// A workspace's changes relative to the point it was branched from --
    /// "what did this agent actually do" in one call. See DESIGN.md
    /// ("pact-vcs > Workspace lifecycle") for the merge-base assumption
    /// this relies on.
    pub fn workspace_diff(&self, id: &str) -> Result<WorkspaceDiff> {
        let workspace = self.get_workspace(id)?;

        let merge_base = Command::new("git")
            .args(["merge-base", "HEAD", &workspace.branch])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to spawn `git merge-base`")?;
        let base = String::from_utf8_lossy(&merge_base.stdout).trim().to_string();

        let commit_log = if base.is_empty() {
            String::new()
        } else {
            run_git_text(
                &self.repo_root,
                &["log", "--oneline", &format!("{base}..{}", workspace.branch)],
            )?
        };
        let committed_summary = if base.is_empty() {
            String::new()
        } else {
            run_git_text(
                &self.repo_root,
                &["diff", "--stat", &format!("{base}..{}", workspace.branch)],
            )?
        };

        let uncommitted_status = self.dirty_status(&workspace.path)?;
        let uncommitted_summary = run_git_text(&workspace.path, &["diff", "--stat", "HEAD"])?;

        Ok(WorkspaceDiff {
            commit_log,
            committed_summary,
            uncommitted_status,
            uncommitted_summary,
        })
    }

    /// The merge-base a workspace's branch forked from, plus the set of
    /// files it has touched since. Used to detect cross-workspace file
    /// overlap (issue #8) -- see DESIGN.md ("pact-vcs > Workspace
    /// lifecycle").
    pub fn workspace_changes(&self, id: &str) -> Result<WorkspaceChanges> {
        let workspace = self.get_workspace(id)?;

        let merge_base_out = Command::new("git")
            .args(["merge-base", "HEAD", &workspace.branch])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to spawn `git merge-base`")?;
        let merge_base = String::from_utf8_lossy(&merge_base_out.stdout).trim().to_string();

        let mut files = std::collections::BTreeSet::new();

        if !merge_base.is_empty() {
            let committed = run_git_text(
                &self.repo_root,
                &[
                    "diff",
                    "--name-only",
                    &format!("{merge_base}..{}", workspace.branch),
                ],
            )?;
            for line in committed.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    files.insert(line.replace('\\', "/"));
                }
            }
        }

        for line in self.dirty_status(&workspace.path)?.lines() {
            if let Some(path) = parse_porcelain_path(line) {
                files.insert(path);
            }
        }

        Ok(WorkspaceChanges {
            merge_base,
            files: files.into_iter().collect(),
        })
    }

    /// Best-effort: a failure to delete the branch (e.g. it was already
    /// removed, or checked out somewhere else) shouldn't fail the whole
    /// teardown -- the worktree is already gone at this point, which is
    /// the part that actually matters for freeing up the workspace.
    fn delete_branch(&self, branch: &str) {
        let output = Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(&self.repo_root)
            .output();
        match output {
            Ok(o) if !o.status.success() => {
                tracing::warn!(
                    "failed to delete branch '{branch}' after teardown: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(err) => tracing::warn!("failed to spawn git to delete branch '{branch}': {err}"),
            _ => {}
        }
    }

    /// Removes a worktree directory, tolerating two Windows failure modes
    /// -- see DESIGN.md ("pact-vcs > Workspace teardown").
    fn remove_worktree_retrying(&self, path: &std::path::Path) -> Result<()> {
        let mut last_err = String::new();
        for attempt in 0..10 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(300));
            }
            let output = Command::new("git")
                .args(["worktree", "remove"])
                .arg(path)
                .arg("--force")
                .current_dir(&self.repo_root)
                .output()
                .context("failed to spawn `git worktree remove`")?;
            if output.status.success() {
                return Ok(());
            }
            last_err = String::from_utf8_lossy(&output.stderr).to_string();
            if last_err.contains("is not a working tree") {
                break; // git already unregistered it; fall through to a plain directory removal
            }
        }

        if !path.exists() {
            return Ok(());
        }

        for attempt in 0..10 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(300));
            }
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(err) => last_err = err.to_string(),
            }
        }

        bail!("failed to remove worktree directory {}: {last_err}", path.display());
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let mut out = Vec::new();
        let meta_dir = self.state_dir.join("meta");
        for entry in std::fs::read_dir(&meta_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let contents = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                out.push(serde_json::from_str(strip_bom(&contents))?);
            }
        }
        out.sort_by_key(|w: &Workspace| w.created_at);
        Ok(out)
    }

    pub fn get_workspace(&self, id: &str) -> Result<Workspace> {
        let contents = std::fs::read_to_string(self.meta_path(id))
            .with_context(|| format!("no workspace found with id '{id}'"))?;
        Ok(serde_json::from_str(strip_bom(&contents))?)
    }

    /// Stages and commits everything in a workspace's working tree with a
    /// message derived from its task text -- see DESIGN.md ("pact-vcs >
    /// commit_all"). Returns `Ok(false)` without running `git commit` at
    /// all if the workspace is already clean.
    pub fn commit_all(&self, id: &str) -> Result<bool> {
        let workspace = self.get_workspace(id)?;
        if self.dirty_status(&workspace.path)?.is_empty() {
            return Ok(false);
        }

        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&workspace.path)
            .output()
            .context("failed to spawn `git add`")?;
        if !add.status.success() {
            bail!(
                "git add failed in {}:\n{}",
                workspace.path.display(),
                String::from_utf8_lossy(&add.stderr)
            );
        }

        let message = commit_message(id, &workspace.task);
        let commit = Command::new("git")
            .args(["commit", "-m", &message])
            .current_dir(&workspace.path)
            .output()
            .context("failed to spawn `git commit`")?;
        if !commit.status.success() {
            bail!(
                "git commit failed in {}:\n{}",
                workspace.path.display(),
                String::from_utf8_lossy(&commit.stderr)
            );
        }

        Ok(true)
    }

    /// Closes the loop from "N workspaces are dirty" to "one clean
    /// integration branch" -- see DESIGN.md ("pact-vcs > merge_all") for
    /// the phase breakdown and the trial report this is built against.
    /// `ids`, if given, restricts the run to those workspaces instead of
    /// every active one. `require_passing_tests`, if given, gates each
    /// workspace's *clean* merge on this command passing in the
    /// integration worktree before it's accepted -- see DESIGN.md
    /// ("pact-vcs > Test-gated merge (issue #65)").
    pub fn merge_all(
        &self,
        ids: Option<&[String]>,
        target_branch: Option<&str>,
        union_globs: &[String],
        arbiter: Option<&ArbiterResolver<'_>>,
        require_passing_tests: Option<&str>,
        dry_run: bool,
    ) -> Result<MergeReport> {
        let mut selected: Vec<Workspace> = match ids {
            Some(ids) => ids.iter().map(|id| self.get_workspace(id)).collect::<Result<_>>()?,
            None => self.list_workspaces()?,
        };
        if selected.is_empty() {
            bail!("no active workspaces to merge");
        }

        let head = run_git_text(&self.repo_root, &["rev-parse", "HEAD"])?;
        if head.is_empty() {
            bail!("could not resolve current HEAD in {}", self.repo_root.display());
        }

        for workspace in &selected {
            if let Err(err) = self.commit_all(&workspace.id) {
                tracing::warn!(
                    "workspace {}: failed to auto-commit before merge, leaving it out: {err:#}",
                    workspace.id
                );
            }
        }

        let mut skipped = Vec::new();
        selected.retain(|workspace| {
            if workspace.base_commit.is_empty() {
                tracing::warn!(
                    "workspace {} has no recorded base commit (created before this check \
                     existed) -- skipping the moving-base check for it",
                    workspace.id
                );
                return true;
            }
            match self.is_ancestor(&workspace.base_commit, &head) {
                Ok(true) => true,
                Ok(false) => {
                    skipped.push(SkippedWorkspace {
                        id: workspace.id.clone(),
                        branch: workspace.branch.clone(),
                        reason: format!(
                            "base commit {} is no longer part of this branch's history -- \
                             was it reset or rebased since this workspace was created?",
                            short_sha(&workspace.base_commit)
                        ),
                    });
                    false
                }
                Err(err) => {
                    tracing::warn!(
                        "workspace {}: could not verify base ancestry, allowing it through: {err:#}",
                        workspace.id
                    );
                    true
                }
            }
        });

        let mut sized: Vec<(usize, Workspace)> = selected
            .into_iter()
            .map(|w| {
                let n = self
                    .workspace_changes(&w.id)
                    .map(|c| c.files.len())
                    .unwrap_or(usize::MAX);
                (n, w)
            })
            .collect();
        sized.sort_by_key(|(n, _)| *n);

        let branch_name = target_branch
            .map(str::to_string)
            .unwrap_or_else(|| format!("pact/merged-{}", short_id()));

        if dry_run {
            return Ok(MergeReport {
                target_branch: branch_name,
                base_commit: head,
                merged: Vec::new(),
                skipped,
                conflicted: Vec::new(),
                planned: sized.into_iter().map(|(_, w)| w.id).collect(),
                dry_run: true,
            });
        }

        let integration_path = self
            .state_dir
            .join("integration")
            .join(branch_name.replace('/', "-"));

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;
            let output = Command::new("git")
                .args(["worktree", "add"])
                .arg(&integration_path)
                .args(["-b", &branch_name, &head])
                .current_dir(&self.repo_root)
                .output()
                .context("failed to spawn `git worktree add` for the integration branch")?;
            if !output.status.success() {
                bail!(
                    "failed to create integration branch '{branch_name}':\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        let mut merged = Vec::new();
        let mut conflicted = Vec::new();
        for (_, workspace) in sized {
            let commit_before = run_git_text(&integration_path, &["rev-parse", "HEAD"])?;
            match self.merge_branch_into(&integration_path, &workspace.branch, union_globs, arbiter, &workspace.task)? {
                MergeOutcome::Merged { auto_resolved, arbiter_resolved } => {
                    if let Some(test_cmd) = require_passing_tests {
                        if !run_shell(&integration_path, test_cmd)? {
                            self.reset_integration_worktree(&integration_path, &commit_before)?;
                            skipped.push(SkippedWorkspace {
                                id: workspace.id,
                                branch: workspace.branch,
                                reason: format!("merged cleanly but failed the required test command ('{test_cmd}')"),
                            });
                            continue;
                        }
                    }
                    merged.push(MergedWorkspace {
                        id: workspace.id,
                        branch: workspace.branch,
                        auto_resolved,
                        arbiter_resolved,
                    })
                }
                MergeOutcome::Conflict { files } => {
                    skipped.push(SkippedWorkspace {
                        id: workspace.id.clone(),
                        branch: workspace.branch.clone(),
                        reason: format!("merge conflict in: {}", files.join(", ")),
                    });
                    conflicted.push(ConflictedWorkspace {
                        id: workspace.id,
                        branch: workspace.branch,
                        target_branch: branch_name.clone(),
                        files,
                    });
                }
            }
        }

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;
            // No delete_branch call here -- the branch is the actual
            // result and must survive, only the scaffolding worktree goes.
            self.remove_worktree_retrying(&integration_path)?;
        }

        Ok(MergeReport {
            target_branch: branch_name,
            base_commit: head,
            merged,
            skipped,
            conflicted,
            planned: Vec::new(),
            dry_run: false,
        })
    }

    /// Retries merging `workspace_id`'s branch into `target_branch` -- the
    /// "resumable conflict" verb, see DESIGN.md ("pact-vcs > Persisted
    /// conflicts (issue #85)"). `target_branch` must already exist as a
    /// real branch (it does, for anything `merge_all` reported as
    /// conflicted -- see `ConflictedWorkspace::target_branch` -- since
    /// `merge_all` never deletes the branch it built, only the throwaway
    /// worktree). Reuses `merge_branch_into` directly, so a resolution
    /// (auto-resolve, `--union`, or Arbiter) behaves identically to the
    /// original attempt inside `merge_all` -- there's no separate "resolve"
    /// code path to drift out of sync. On success, the commit lands
    /// directly on `target_branch` (the resolve worktree's checked-out
    /// branch *is* `target_branch`, not a copy), so no separate step is
    /// needed to publish the result the way `merge_all` republishes its
    /// own throwaway integration branch.
    pub fn resolve_conflict(
        &self,
        target_branch: &str,
        workspace_id: &str,
        union_globs: &[String],
        arbiter: Option<&ArbiterResolver<'_>>,
    ) -> Result<ResolveOutcome> {
        let workspace = self.get_workspace(workspace_id)?;
        let resolve_path = self.state_dir.join("integration").join(format!("resolve-{}", short_id()));

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;
            let output = Command::new("git")
                .args(["worktree", "add"])
                .arg(&resolve_path)
                .arg(target_branch)
                .current_dir(&self.repo_root)
                .output()
                .context("failed to spawn `git worktree add` for conflict resolution")?;
            if !output.status.success() {
                bail!(
                    "failed to check out target branch '{target_branch}' for conflict resolution:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        let outcome = self.merge_branch_into(&resolve_path, &workspace.branch, union_globs, arbiter, &workspace.task);

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;
            self.remove_worktree_retrying(&resolve_path)?;
        }

        match outcome? {
            MergeOutcome::Merged { auto_resolved, arbiter_resolved } => {
                Ok(ResolveOutcome::Resolved { auto_resolved, arbiter_resolved })
            }
            MergeOutcome::Conflict { files } => Ok(ResolveOutcome::StillConflicted { files }),
        }
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to spawn `git merge-base --is-ancestor`")?;
        Ok(output.status.success())
    }

    /// Merges `branch` into whatever's checked out in `worktree_path`
    /// (always the throwaway integration worktree, never the repo's own
    /// checkout) -- see DESIGN.md ("pact-vcs > Semantic auto-resolution").
    fn merge_branch_into(
        &self,
        worktree_path: &Path,
        branch: &str,
        union_globs: &[String],
        arbiter: Option<&ArbiterResolver<'_>>,
        task_text: &str,
    ) -> Result<MergeOutcome> {
        let output = Command::new("git")
            .args(["merge", "--no-edit", branch])
            .current_dir(worktree_path)
            .output()
            .with_context(|| format!("failed to spawn `git merge {branch}`"))?;
        if output.status.success() {
            return Ok(MergeOutcome::Merged { auto_resolved: Vec::new(), arbiter_resolved: Vec::new() });
        }

        let status = self.dirty_status(worktree_path)?;
        let conflicted_files: Vec<String> = status
            .lines()
            .filter(|line| {
                matches!(
                    line.get(0..2),
                    Some("UU") | Some("AA") | Some("DD") | Some("AU") | Some("UA") | Some("UD") | Some("DU")
                )
            })
            .filter_map(parse_porcelain_path)
            .collect();

        if conflicted_files.is_empty() {
            self.abort_merge(worktree_path, branch);
            return Ok(MergeOutcome::Conflict {
                files: vec![String::from_utf8_lossy(&output.stderr).trim().to_string()],
            });
        }

        let mut unresolved = Vec::new();
        let mut auto_resolved = Vec::new();
        for file in &conflicted_files {
            match self.try_auto_resolve(worktree_path, file, union_globs) {
                Ok(true) => auto_resolved.push(file.clone()),
                Ok(false) => unresolved.push(file.clone()),
                Err(err) => {
                    tracing::warn!(
                        "auto-resolve attempt on '{file}' (branch '{branch}') failed, leaving it \
                         conflicted: {err:#}"
                    );
                    unresolved.push(file.clone());
                }
            }
        }

        let mut arbiter_resolved = Vec::new();
        if !unresolved.is_empty() {
            if let Some(resolve) = arbiter {
                let resolved_by_arbiter = resolve(worktree_path, task_text, &unresolved);
                unresolved.retain(|file| !resolved_by_arbiter.contains(file));
                arbiter_resolved = resolved_by_arbiter;
            }
        }

        if unresolved.is_empty() {
            let commit = Command::new("git")
                .args(["commit", "--no-edit"])
                .current_dir(worktree_path)
                .output()
                .context("failed to spawn `git commit` after auto-resolving a merge")?;
            if commit.status.success() {
                return Ok(MergeOutcome::Merged { auto_resolved, arbiter_resolved });
            }
            tracing::warn!(
                "auto-resolved every conflicted file for branch '{branch}' but `git commit` \
                 still failed -- treating as a genuine conflict instead: {}",
                String::from_utf8_lossy(&commit.stderr)
            );
            unresolved = conflicted_files;
        }

        self.abort_merge(worktree_path, branch);
        Ok(MergeOutcome::Conflict { files: unresolved })
    }

    fn abort_merge(&self, worktree_path: &Path, branch: &str) {
        let abort = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(worktree_path)
            .output();
        if let Ok(abort) = abort {
            if !abort.status.success() {
                tracing::warn!(
                    "`git merge --abort` in {} failed after a conflict on branch '{branch}': {}",
                    worktree_path.display(),
                    String::from_utf8_lossy(&abort.stderr)
                );
            }
        }
    }

    /// Undoes one accepted-then-rejected merge in `--require-passing-tests`
    /// gating (issue #65) -- a plain hard reset back to the commit the
    /// integration worktree was at before this workspace's merge landed.
    /// Safe specifically because this worktree is never shared with
    /// anything else (unlike the repo's own checkout), so nothing else can
    /// observe the now-discarded commit in between.
    fn reset_integration_worktree(&self, worktree_path: &Path, commit_before: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["reset", "--hard", commit_before])
            .current_dir(worktree_path)
            .output()
            .context("failed to spawn `git reset --hard` after a failed test-gate")?;
        if !output.status.success() {
            bail!(
                "failed to reset the integration worktree after a failed test-gate:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    /// Tries to auto-resolve one conflicted file, returning `true` and
    /// staging it (`git add`) if it resolved -- see DESIGN.md ("pact-vcs >
    /// Semantic auto-resolution").
    fn try_auto_resolve(&self, worktree_path: &Path, file: &str, union_globs: &[String]) -> Result<bool> {
        if is_never_auto_resolve(file) {
            return Ok(false);
        }

        let resolved = if is_package_json(file) {
            self.try_resolve_package_json(worktree_path, file)?
        } else if union_globs.iter().any(|pattern| glob_matches(pattern, file)) {
            self.try_resolve_union(worktree_path, file)?
        } else {
            None
        };

        let Some(content) = resolved else {
            return Ok(false);
        };

        std::fs::write(worktree_path.join(file), content)
            .with_context(|| format!("writing auto-resolved content for {file}"))?;
        let add = Command::new("git")
            .args(["add", "--", file])
            .current_dir(worktree_path)
            .output()
            .with_context(|| format!("failed to spawn `git add` for auto-resolved {file}"))?;
        Ok(add.status.success())
    }

    /// Reads one side of a conflicted file from git's index -- stage 1 is
    /// the common ancestor, 2 is "ours", 3 is "theirs". `Ok(None)` if that
    /// stage doesn't exist for this path. Strips a leading UTF-8 BOM if
    /// present -- issue #57: PowerShell's `Out-File -Encoding utf8` (and
    /// other common Windows tooling) writes one by default, and
    /// `serde_json::from_str` rejects a BOM outright, so an otherwise valid
    /// `package.json` stage would silently fail `try_resolve_package_json`'s
    /// parse and fall through to a real conflict instead of auto-resolving.
    ///
    /// The returned `bool` reports whether a BOM was present on this stage
    /// before it was stripped -- issue #79: stripping it here for parsing
    /// is correct, but the caller needs to know it was there in the first
    /// place to restore it on write, or a BOM'd `package.json` gets
    /// silently normalized to non-BOM on the first merge that needs
    /// conflict resolution.
    fn read_conflict_stage(&self, worktree_path: &Path, file: &str, stage: u8) -> Result<Option<(String, bool)>> {
        let output = Command::new("git")
            .args(["show", &format!(":{stage}:{file}")])
            .current_dir(worktree_path)
            .output()
            .context("failed to spawn `git show` for a conflicted file's stage")?;
        if !output.status.success() {
            return Ok(None);
        }
        let content = String::from_utf8_lossy(&output.stdout);
        let had_bom = content.starts_with('\u{FEFF}');
        Ok(Some((strip_bom(&content).to_string(), had_bom)))
    }

    /// JSON-aware merge of `package.json`'s dependency blocks -- see
    /// DESIGN.md ("pact-vcs > Semantic auto-resolution").
    fn try_resolve_package_json(&self, worktree_path: &Path, file: &str) -> Result<Option<String>> {
        let (Some((base, _)), Some((ours, ours_had_bom)), Some((theirs, _))) = (
            self.read_conflict_stage(worktree_path, file, 1)?,
            self.read_conflict_stage(worktree_path, file, 2)?,
            self.read_conflict_stage(worktree_path, file, 3)?,
        ) else {
            return Ok(None);
        };

        let (Ok(base), Ok(ours_value), Ok(theirs_value)) = (
            serde_json::from_str::<serde_json::Value>(&base),
            serde_json::from_str::<serde_json::Value>(&ours),
            serde_json::from_str::<serde_json::Value>(&theirs),
        ) else {
            return Ok(None);
        };

        let mut ours_stripped = ours_value.clone();
        let mut theirs_stripped = theirs_value.clone();
        if let (Some(o), Some(t)) = (ours_stripped.as_object_mut(), theirs_stripped.as_object_mut()) {
            for key in PACKAGE_JSON_DEP_KEYS {
                o.remove(*key);
                t.remove(*key);
            }
        }
        if ours_stripped != theirs_stripped {
            return Ok(None);
        }

        let Some(mut merged_obj) = ours_value.as_object().cloned() else {
            return Ok(None);
        };

        for key in PACKAGE_JSON_DEP_KEYS {
            let base_block = base.get(*key).and_then(|v| v.as_object());
            let ours_block = ours_value.get(*key).and_then(|v| v.as_object());
            let theirs_block = theirs_value.get(*key).and_then(|v| v.as_object());
            if base_block.is_none() && ours_block.is_none() && theirs_block.is_none() {
                continue;
            }

            let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            if let Some(m) = ours_block {
                names.extend(m.keys().cloned());
            }
            if let Some(m) = theirs_block {
                names.extend(m.keys().cloned());
            }

            let mut merged_block = serde_json::Map::new();
            for name in names {
                let base_v = base_block.and_then(|m| m.get(&name));
                let ours_v = ours_block.and_then(|m| m.get(&name));
                let theirs_v = theirs_block.and_then(|m| m.get(&name));
                let resolved = match (ours_v, theirs_v) {
                    (Some(o), Some(t)) if o == t => o.clone(),
                    (Some(o), Some(t)) => {
                        if base_v == Some(o) {
                            t.clone() // only theirs changed this dependency
                        } else if base_v == Some(t) {
                            o.clone() // only ours changed this dependency
                        } else {
                            return Ok(None); // both changed it, differently
                        }
                    }
                    (Some(o), None) => o.clone(),
                    (None, Some(t)) => t.clone(),
                    (None, None) => unreachable!("name came from ours_block or theirs_block"),
                };
                merged_block.insert(name, resolved);
            }
            merged_obj.insert(key.to_string(), serde_json::Value::Object(merged_block));
        }

        let merged_value = serde_json::Value::Object(merged_obj);

        // `to_string_pretty` alone would do two things this resolver isn't
        // supposed to do: reorder every top-level key alphabetically
        // (serde_json's `Value::Object` is a plain `serde_json::Map`, which
        // without the `preserve_order` feature is BTreeMap-backed) and
        // hardcode 2-space indent regardless of the file's own convention.
        // `merged_obj` above is built by cloning `ours_value`'s object and
        // updating entries in place, so with `preserve_order` on, its key
        // order already matches "ours" -- this only needs to match the
        // indent width, not touch ordering.
        let indent = detect_json_indent(&ours);
        let mut buf = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(&indent);
        let mut serializer = serde_json::Serializer::with_formatter(&mut buf, formatter);
        serde::Serialize::serialize(&merged_value, &mut serializer)
            .context("serializing auto-resolved package.json")?;
        let mut result =
            String::from_utf8(buf).context("auto-resolved package.json was not valid UTF-8")?;
        result.push('\n');

        // "ours" is the integration branch's existing convention (same
        // reasoning `detect_json_indent(&ours)` above already uses for
        // indent width) -- if its committed package.json had a BOM,
        // restore it here. Otherwise the merged output silently drops it,
        // even though nothing about resolving the dependency-block
        // conflict was ever meant to change the file's encoding (issue
        // #79).
        if ours_had_bom {
            result.insert(0, '\u{FEFF}');
        }

        Ok(Some(result))
    }

    /// Plain line-union merge for a `--union`-matched file -- see
    /// DESIGN.md ("pact-vcs > Semantic auto-resolution").
    fn try_resolve_union(&self, worktree_path: &Path, file: &str) -> Result<Option<String>> {
        let (Some((ours, _)), Some((theirs, _))) = (
            self.read_conflict_stage(worktree_path, file, 2)?,
            self.read_conflict_stage(worktree_path, file, 3)?,
        ) else {
            return Ok(None);
        };

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut merged_lines: Vec<&str> = Vec::new();
        for line in ours.lines() {
            if seen.insert(line) {
                merged_lines.push(line);
            }
        }
        for line in theirs.lines() {
            if seen.insert(line) {
                merged_lines.push(line);
            }
        }

        let mut result = merged_lines.join("\n");
        result.push('\n');

        // A plain line-concat is wrong for any file with "final
        // assignment/declaration wins" semantics: two independent barrel
        // appends can each be a no-conflict-looking line, yet together
        // produce two `module.exports =` statements (second silently wins,
        // first is dropped) or two declarations binding the same
        // identifier (a real redeclaration SyntaxError in JS/TS). Confirmed
        // by hand: this exact shape reliably breaks a merged CommonJS
        // barrel. Treat that as "don't understand this well enough to
        // auto-resolve" rather than reporting a broken merge as a success.
        if !union_merge_is_safe(file, &result) {
            return Ok(None);
        }

        Ok(Some(result))
    }
}

/// File extensions `try_resolve_union`'s safety check applies to.
const UNION_SAFETY_CHECKED_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "tsx"];

/// Heuristic (not a real parser) check for the two `--union` failure modes
/// found in practice on JS/TS files: two `module.exports =` / `export
/// default` statements surviving into the same merged file, and two
/// declarations binding the same identifier in the same scope. False
/// negatives are possible by design (this is intentionally cheap, not a
/// full parser); a false positive just means a file that would otherwise
/// silently break instead falls through to "needs a human", which is the
/// safe direction. Non-JS/TS files are never checked.
fn union_merge_is_safe(file: &str, content: &str) -> bool {
    let ext = Path::new(file).extension().and_then(|e| e.to_str()).unwrap_or("");
    if !UNION_SAFETY_CHECKED_EXTENSIONS.contains(&ext) {
        return true;
    }

    let mut module_exports_count = 0u32;
    let mut default_export_count = 0u32;
    let mut bound_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        if let Some(rest) = line.strip_prefix("module.exports") {
            let rest = rest.trim_start();
            if rest.starts_with('=') && !rest.starts_with("==") {
                module_exports_count += 1;
            }
        }
        if line.starts_with("export default ") || line == "export default" || line == "export default;" {
            default_export_count += 1;
        }

        for keyword in ["const ", "let ", "var "] {
            if let Some(rest) = line.strip_prefix(keyword) {
                for name in binding_names(rest) {
                    if !bound_names.insert(name) {
                        return false;
                    }
                }
            }
        }
    }

    module_exports_count <= 1 && default_export_count <= 1
}

/// Extracts the identifier(s) a single `const`/`let`/`var` declaration
/// binds, from the source text right after the keyword -- handles a plain
/// identifier (`x = ...`), object destructuring (`{ a, b: c, ...rest } =
/// ...`), and array destructuring (`[a, , b] = ...`). Best-effort: only
/// needs to catch the common barrel-export shape, not be a full parser.
fn binding_names(rest: &str) -> Vec<String> {
    let rest = rest.trim_start();
    let extract = |inner: &str| -> Vec<String> {
        inner
            .split(',')
            .filter_map(|entry| {
                let entry = entry.trim().trim_start_matches("...").trim();
                let key = entry.split(':').next().unwrap_or(entry).trim();
                let key = key.split('=').next().unwrap_or(key).trim();
                if key.is_empty() {
                    None
                } else {
                    Some(key.to_string())
                }
            })
            .collect()
    };

    if let Some(inner) = rest.strip_prefix('{') {
        match inner.find('}') {
            Some(end) => extract(&inner[..end]),
            None => Vec::new(),
        }
    } else if let Some(inner) = rest.strip_prefix('[') {
        match inner.find(']') {
            Some(end) => extract(&inner[..end]),
            None => Vec::new(),
        }
    } else {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        if name.is_empty() {
            Vec::new()
        } else {
            vec![name]
        }
    }
}

/// Sniffs the indent unit (spaces or a tab) from the first indented line of
/// a JSON file's text, so a re-serialized merge matches the file's own
/// convention instead of always hardcoding 2 spaces. Falls back to 2
/// spaces (serde_json's own default) if nothing indented is found, e.g. a
/// single-line/minified file.
fn detect_json_indent(text: &str) -> Vec<u8> {
    for line in text.lines() {
        let stripped = line.trim_start_matches([' ', '\t']);
        let indent_len = line.len() - stripped.len();
        if indent_len > 0 && !stripped.is_empty() {
            return line.as_bytes()[..indent_len].to_vec();
        }
    }
    b"  ".to_vec()
}

/// Strips a leading UTF-8 BOM (`\u{FEFF}`), if present. Common Windows
/// tooling (PowerShell's `Out-File -Encoding utf8`, some editors) writes one
/// by default; `serde_json::from_str` rejects it outright, which otherwise
/// silently breaks `try_resolve_package_json`'s parse on an otherwise valid
/// file (issue #57).
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

fn is_never_auto_resolve(path: &str) -> bool {
    let basename = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    NEVER_AUTO_RESOLVE.contains(&basename)
}

fn is_package_json(path: &str) -> bool {
    Path::new(path).file_name().and_then(|n| n.to_str()) == Some("package.json")
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .ok()
        .map(|g| g.compile_matcher().is_match(path))
        .unwrap_or(false)
}

fn commit_message(id: &str, task: &str) -> String {
    let task = task.trim();
    let first_line = task.lines().next().unwrap_or(task).trim();

    let (subject_line, truncated) = if first_line.chars().count() > 72 {
        let truncated: String = first_line.chars().take(69).collect();
        (format!("{truncated}..."), true)
    } else {
        (first_line.to_string(), false)
    };
    let subject = format!("agent {id}: {subject_line}");

    if task == first_line && !truncated {
        subject
    } else {
        format!("{subject}\n\n{task}")
    }
}

/// If `workspace` has a live agent process recorded, kills its whole
/// process tree before the worktree it's running in gets removed out from
/// under it -- see DESIGN.md ("pact-agents > Process group kill").
/// Best-effort: a dead/already-exited PID is silently ignored, not an
/// error.
fn kill_if_alive(workspace: &Workspace) {
    let Some(pid) = workspace.agent_pid else {
        return;
    };
    let sys_pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    if sys.process(sys_pid).is_none() {
        return;
    }

    tracing::info!("killing agent process {pid} (and its children) before removing its workspace");
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    } else {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    for _ in 0..20 {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        if sys.process(sys_pid).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs `git <args>` in `dir` and returns stdout as text, tolerating a
/// non-zero exit (e.g. `diff --stat` against a ref with no differences is
/// still success, but callers here care about "no meaningful output" more
/// than "git considered this an error").
/// Runs `cmd` as a shell command in `dir` (`cmd /C` on Windows, `sh -c`
/// elsewhere), returning whether it exited successfully -- see DESIGN.md
/// ("pact-vcs > Test-gated merge (issue #65)"). A local copy of the same
/// small helper `pact-core`'s Arbiter uses (`run_shell`), not shared: this
/// crate has no dependency on `pact-core` and the alternative -- adding
/// one just for a 15-line function -- would be backwards, `pact-core`
/// depends on `pact-vcs`, not the other way around.
fn run_shell(dir: &Path, cmd: &str) -> Result<bool> {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    let output = command
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to spawn required test command '{cmd}'"))?;
    Ok(output.status.success())
}

fn run_git_text(dir: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

/// Extracts the file path from one `git status --porcelain` line (format:
/// two status chars, a space, then the path -- or, for a rename,
/// `orig -> new`, where only the new path matters here).
fn parse_porcelain_path(line: &str) -> Option<String> {
    let rest = line.get(3..)?;
    let path = match rest.find(" -> ") {
        Some(idx) => &rest[idx + 4..],
        None => rest,
    };
    let path = path.trim();
    if path.is_empty() {
        None
    } else {
        Some(path.replace('\\', "/"))
    }
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// First 12 chars of a commit sha for a human-readable report line --
/// doesn't assume the input is exactly 40 chars (already-short input, e.g.
/// from a test, is returned as-is).
fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_message_short_single_line_task() {
        assert_eq!(
            commit_message("ab12cd34", "add chunk.ts utility"),
            "agent ab12cd34: add chunk.ts utility"
        );
    }

    #[test]
    fn commit_message_trims_surrounding_whitespace() {
        assert_eq!(
            commit_message("ab12cd34", "  add chunk.ts utility  \n"),
            "agent ab12cd34: add chunk.ts utility"
        );
    }

    #[test]
    fn commit_message_truncates_long_subject_and_keeps_full_body() {
        let task = "a".repeat(100);
        let message = commit_message("ab12cd34", &task);
        let mut lines = message.lines();
        let subject = lines.next().unwrap();

        assert!(subject.chars().count() <= 72 + "agent ab12cd34: ...".len());
        assert!(subject.ends_with("..."));
        assert!(message.ends_with(&task));
    }

    #[test]
    fn commit_message_multiline_task_keeps_full_text_in_body() {
        let task = "add chunk.ts utility\n\nHandles the empty-array edge case explicitly.";
        let message = commit_message("ab12cd34", task);
        assert_eq!(message.lines().next().unwrap(), "agent ab12cd34: add chunk.ts utility");
        assert!(message.contains("Handles the empty-array edge case explicitly."));
    }

    #[test]
    fn parse_porcelain_path_plain_entry() {
        assert_eq!(
            parse_porcelain_path(" M src/index.ts"),
            Some("src/index.ts".to_string())
        );
    }

    #[test]
    fn parse_porcelain_path_rename_entry_uses_new_path() {
        assert_eq!(
            parse_porcelain_path("R  src/old.ts -> src/new.ts"),
            Some("src/new.ts".to_string())
        );
    }

    #[test]
    fn parse_porcelain_path_normalizes_backslashes() {
        assert_eq!(
            parse_porcelain_path(" M src\\nested\\file.ts"),
            Some("src/nested/file.ts".to_string())
        );
    }

    #[test]
    fn union_merge_is_safe_ignores_non_js_ts_files() {
        // Two "final value wins" assignments, but this isn't a checked
        // extension, so the safety check doesn't apply.
        let content = "module.exports = { a };\nmodule.exports = { b };\n";
        assert!(union_merge_is_safe("CHANGELOG.md", content));
    }

    #[test]
    fn union_merge_is_safe_accepts_plain_barrel_append() {
        let content = "export {};\nexport * from './chunk';\nexport * from './omit';\n";
        assert!(union_merge_is_safe("src/barrel.ts", content));
    }

    #[test]
    fn union_merge_rejects_duplicate_module_exports() {
        let content = "const { mul } = require('./mul');\n\
                        const { div } = require('./div');\n\
                        module.exports = { mul };\n\
                        module.exports = { div };\n";
        assert!(!union_merge_is_safe("src/index.js", content));
    }

    #[test]
    fn union_merge_rejects_redeclared_destructured_binding() {
        let content = "const { add, sub, mul } = require('../src/index');\n\
                        const { add, sub, div } = require('../src/index');\n";
        assert!(!union_merge_is_safe("test/index.test.js", content));
    }

    #[test]
    fn union_merge_rejects_duplicate_export_default() {
        let content = "export default class A {}\nexport default class B {}\n";
        assert!(!union_merge_is_safe("src/widget.tsx", content));
    }

    #[test]
    fn union_merge_allows_module_exports_property_assignment() {
        // `module.exports.foo = ...` is not a full reassignment, so two of
        // these (for different properties) is a legitimate union merge.
        let content = "module.exports.mul = require('./mul');\nmodule.exports.div = require('./div');\n";
        assert!(union_merge_is_safe("src/index.js", content));
    }

    #[test]
    fn detect_json_indent_finds_two_space() {
        let text = "{\n  \"a\": 1\n}\n";
        assert_eq!(detect_json_indent(text), b"  ".to_vec());
    }

    #[test]
    fn detect_json_indent_finds_four_space() {
        let text = "{\n    \"a\": 1\n}\n";
        assert_eq!(detect_json_indent(text), b"    ".to_vec());
    }

    #[test]
    fn detect_json_indent_finds_tab() {
        let text = "{\n\t\"a\": 1\n}\n";
        assert_eq!(detect_json_indent(text), b"\t".to_vec());
    }

    #[test]
    fn detect_json_indent_falls_back_to_two_space_for_minified_json() {
        let text = "{\"a\":1}";
        assert_eq!(detect_json_indent(text), b"  ".to_vec());
    }

    #[test]
    fn strip_bom_removes_a_leading_bom() {
        assert_eq!(strip_bom("\u{FEFF}{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn strip_bom_is_a_no_op_without_one() {
        assert_eq!(strip_bom("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn strip_bom_only_removes_a_leading_occurrence() {
        // A BOM character anywhere but the start is real (unlikely, but not
        // this function's job to touch) content, not a byte-order mark.
        let s = "{\"a\":\"\u{FEFF}\"}";
        assert_eq!(strip_bom(s), s);
    }
}
