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
    /// Populated only for `--dry-run`: the merge order that *would* be
    /// used, after sequencing and the moving-base check, without any git
    /// state actually being touched.
    pub planned: Vec<String>,
    pub dry_run: bool,
}

enum MergeOutcome {
    Merged { auto_resolved: Vec<String> },
    Conflict { files: Vec<String> },
}

/// package.json's dependency-ish top-level keys -- the only part of the
/// file `try_resolve_package_json` will touch. A conflict anywhere else in
/// the file (scripts, version, name, ...) is left as a real conflict, same
/// as before this existed.
const PACKAGE_JSON_DEP_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

/// Basenames pact never attempts to auto-resolve, even under `--union` --
/// generated files with integrity hashes or resolved dependency graphs,
/// where a naive line-level merge is very likely to silently produce a
/// corrupt result. A real conflict here always stays a real conflict for a
/// human (or a regenerate step) to handle.
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

    pub fn create_workspace(&self, task: &str) -> Result<Workspace> {
        let id = short_id();
        let branch = format!("pact/{id}");
        let path = self.state_dir.join("workspaces").join(&id);

        let base_commit = {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;

            // Captured under the same lock as `worktree add` immediately
            // below, so this is exactly the commit the new branch forks
            // from -- not a value that could race against a concurrent
            // `pact spawn` moving HEAD in between the two calls.
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
    /// the `pact/<id>` branch created for it. Confirmed via a real trial
    /// run (an outside reviewer's report): `git worktree remove` does not
    /// delete the branch it was created with -- that's standard git
    /// behavior, worktree removal and branch deletion are independent --
    /// so without this, every torn-down workspace left a dead branch
    /// behind, accumulating over repeated use. Force-deletes (`-D`, not
    /// `-d`) since an agent's throwaway workspace branch is very often
    /// unmerged; `keep_branch` exists for anyone who wants to inspect or
    /// rebase a workspace's commits after tearing it down.
    ///
    /// Refuses on a workspace with uncommitted changes unless `force` is
    /// set. This wasn't a real check before -- confirmed directly, by
    /// spawning a workspace, adding an uncommitted file to it, and running
    /// the old unconditional-`--force` teardown: the file was silently
    /// gone afterward, with no warning at all. The underlying
    /// `git worktree remove` call already has this exact protection built
    /// in (it refuses on a dirty worktree unless *it's* passed `--force`);
    /// this crate's `remove_worktree_retrying` was defeating that
    /// protection unconditionally on every call. This check restores it at
    /// `pact`'s own layer instead, so `--force` is something the caller
    /// chooses, not something baked in silently.
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

    /// A workspace's changes relative to the point it was branched from,
    /// covering both what's committed on its branch and what's still only
    /// in its working tree -- "what did this agent actually do" in one
    /// call, so a user can decide whether to keep, discard, or manually
    /// merge it before tearing it down.
    ///
    /// The merge-base is computed against the *repo root's* current HEAD,
    /// not a persisted value -- correct as long as the repo's own branch
    /// hasn't been reset past the point this workspace's branch forked
    /// from, which is the same assumption `git worktree`/`git worktree
    /// remove` themselves make about a branch's relationship to its
    /// origin.
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
    /// files it has touched since -- both committed on the branch and
    /// still only in its working tree. Used to detect cross-workspace file
    /// overlap (issue #8): two workspaces sharing the same merge-base
    /// forked from a comparable point in history, so any file both of them
    /// touched is worth surfacing, without needing semantic/AST-level
    /// analysis -- file-path overlap is the same restriction the MCP
    /// lease layer already accepts.
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

    /// Removes a worktree directory, tolerating the two Windows failure
    /// modes confirmed against a real killed agent process (not
    /// theoretical): (1) killing a process doesn't mean its handles on its
    /// own `current_dir` are released the instant `kill()` returns, so an
    /// immediate `git worktree remove` can fail with "Permission denied"
    /// even though the process is already gone -- retrying briefly usually
    /// clears this; (2) git unregisters a worktree from its own metadata
    /// *before* attempting to delete the directory, so if that deletion
    /// fails, a later `git worktree remove` on the same path fails with
    /// "is not a working tree" even though the directory (and whatever's
    /// in it) is still sitting there orphaned. In that case this falls
    /// back to removing the directory directly, also with retries, since
    /// it's the same underlying handle-release race, just past the point
    /// where git itself can help.
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
                out.push(serde_json::from_str(&contents)?);
            }
        }
        out.sort_by_key(|w: &Workspace| w.created_at);
        Ok(out)
    }

    pub fn get_workspace(&self, id: &str) -> Result<Workspace> {
        let contents = std::fs::read_to_string(self.meta_path(id))
            .with_context(|| format!("no workspace found with id '{id}'"))?;
        Ok(serde_json::from_str(&contents)?)
    }

    /// Stages and commits everything in a workspace's working tree (staged,
    /// unstaged, and untracked) with a message derived from its task text,
    /// so `pact diff`/`pact log` and `merge-all` always have a real commit
    /// to work with instead of a permanently-dirty worktree -- see the
    /// trial report that motivated this (every workspace ended `[dirty]`
    /// with nothing to merge). Returns `Ok(false)` without running `git
    /// commit` at all if the workspace is already clean, so callers (e.g.
    /// `merge-all`'s first phase) can call this unconditionally on every
    /// workspace.
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
    /// integration branch" (see the trial report this is built against: 9
    /// of 10 manual merges failed on a shared barrel file, and strict-mode
    /// git blocked every merge after the first conflict). Never touches the
    /// repo's own checkout -- everything happens in a throwaway worktree,
    /// same isolation model as agent workspaces themselves, so this is safe
    /// to run regardless of what branch (or branch-protection rules) the
    /// main checkout has.
    ///
    /// Phases, all best-effort (one workspace's failure never blocks
    /// another's): (1) auto-commit every selected workspace via
    /// `commit_all`; (2) moving-base check -- refuse a workspace whose
    /// recorded `base_commit` is no longer an ancestor of current HEAD, so
    /// merging never silently assumes a fork point that isn't real anymore
    /// (e.g. HEAD was reset since the workspace was created); (3) sequence
    /// the rest smallest-changeset-first, on the theory that landing small
    /// compatible changes before a large one reduces cascade conflicts; (4)
    /// merge each into a fresh `target_branch` (default `pact/merged-<id>`)
    /// one at a time, skipping (not aborting the whole run on) a real
    /// conflict.
    ///
    /// `ids`, if given, restricts the run to those workspaces instead of
    /// every active one. `dry_run` runs phases 1-3 (auto-commit still
    /// happens -- see its own doc comment for why that's always safe to
    /// call) but stops before touching git state for the actual merge,
    /// returning the planned order instead.
    pub fn merge_all(
        &self,
        ids: Option<&[String]>,
        target_branch: Option<&str>,
        union_globs: &[String],
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

        // Smallest changeset first -- a workspace whose changes can't be
        // sized (e.g. `workspace_changes` failed) sorts last rather than
        // being dropped, so a bug in sizing never silently excludes it.
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
        for (_, workspace) in sized {
            match self.merge_branch_into(&integration_path, &workspace.branch, union_globs)? {
                MergeOutcome::Merged { auto_resolved } => merged.push(MergedWorkspace {
                    id: workspace.id,
                    branch: workspace.branch,
                    auto_resolved,
                }),
                MergeOutcome::Conflict { files } => skipped.push(SkippedWorkspace {
                    id: workspace.id,
                    branch: workspace.branch,
                    reason: format!("merge conflict in: {}", files.join(", ")),
                }),
            }
        }

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;
            // keep_branch semantics: the worktree was only ever scaffolding
            // to run `git merge` in -- the branch it built up is the actual
            // result and must survive this call.
            self.remove_worktree_retrying(&integration_path)?;
        }

        Ok(MergeReport {
            target_branch: branch_name,
            base_commit: head,
            merged,
            skipped,
            planned: Vec::new(),
            dry_run: false,
        })
    }

    /// `true` if `ancestor` is still part of `descendant`'s history --
    /// i.e. `merge_all`'s recorded base commit for a workspace hasn't been
    /// reset/rebased away since. `git merge-base --is-ancestor` exits
    /// non-zero for "not an ancestor", which is a normal, expected outcome
    /// here, not a spawn/IO failure -- so this returns `Ok(false)` for that
    /// case rather than treating a non-zero exit as an error.
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
    /// checkout). On a real conflict, tries the semantic-narrow
    /// auto-resolution rules (`try_auto_resolve`) on each conflicted file
    /// before giving up: if *every* conflicted file resolves, the merge is
    /// completed with a commit instead of aborted. If any file is left
    /// over, the whole merge is aborted (so the worktree is clean for the
    /// *next* workspace's attempt -- one conflicted workspace must not
    /// poison the rest of the batch) and reported as a real conflict, same
    /// as before this existed.
    fn merge_branch_into(
        &self,
        worktree_path: &Path,
        branch: &str,
        union_globs: &[String],
    ) -> Result<MergeOutcome> {
        let output = Command::new("git")
            .args(["merge", "--no-edit", branch])
            .current_dir(worktree_path)
            .output()
            .with_context(|| format!("failed to spawn `git merge {branch}`"))?;
        if output.status.success() {
            return Ok(MergeOutcome::Merged { auto_resolved: Vec::new() });
        }

        let status = self.dirty_status(worktree_path)?;
        let conflicted_files: Vec<String> = status
            .lines()
            .filter(|line| {
                // Unmerged-path porcelain codes -- the ones `git merge`
                // actually leaves behind on conflict, as opposed to e.g. a
                // plain modified/added entry.
                matches!(
                    line.get(0..2),
                    Some("UU") | Some("AA") | Some("DD") | Some("AU") | Some("UA") | Some("UD") | Some("DU")
                )
            })
            .filter_map(parse_porcelain_path)
            .collect();

        if conflicted_files.is_empty() {
            // `git merge` failed but left no unmerged paths -- not a
            // content conflict (e.g. a dirty-worktree refusal), nothing for
            // auto-resolution to work with.
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

        if unresolved.is_empty() {
            let commit = Command::new("git")
                .args(["commit", "--no-edit"])
                .current_dir(worktree_path)
                .output()
                .context("failed to spawn `git commit` after auto-resolving a merge")?;
            if commit.status.success() {
                return Ok(MergeOutcome::Merged { auto_resolved });
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

    /// Tries to auto-resolve one conflicted file during a merge, using the
    /// semantic-narrow rules `merge_all` is built around: never touch a
    /// generated/structural file (`NEVER_AUTO_RESOLVE`); JSON-aware merge
    /// for `package.json`'s dependency blocks; a plain line-union merge for
    /// anything matching a caller-supplied `--union` glob (nothing is
    /// union-merged unless the caller explicitly named it -- pact does not
    /// guess which files are safe to blindly concatenate). Returns `true`
    /// and stages the file (`git add`) if it resolved, `false` (file left
    /// untouched, still conflicted) otherwise.
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
    /// the common ancestor, 2 is "ours" (the integration branch, before
    /// this merge), 3 is "theirs" (the branch being merged in). Returns
    /// `Ok(None)` if that stage doesn't exist for this path (e.g. the file
    /// was added fresh on only one side), which callers treat as "don't
    /// understand this shape well enough to auto-resolve" rather than an
    /// error.
    fn read_conflict_stage(&self, worktree_path: &Path, file: &str, stage: u8) -> Result<Option<String>> {
        let output = Command::new("git")
            .args(["show", &format!(":{stage}:{file}")])
            .current_dir(worktree_path)
            .output()
            .context("failed to spawn `git show` for a conflicted file's stage")?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
    }

    /// JSON-aware merge of `package.json`'s dependency blocks
    /// (`PACKAGE_JSON_DEP_KEYS`) -- far and away the most common real
    /// conflict from parallel agents (two agents both adding a package). A
    /// dependency name added or changed on exactly one side is taken as-is;
    /// changed to the *same* value on both sides is fine; changed to
    /// *different* values on both sides is a real conflict this does not
    /// try to guess at -- returns `Ok(None)` for the whole file in that
    /// case, same as if anything *outside* the dependency keys differs
    /// between the two sides. This only ever resolves the dependency-block
    /// class of conflict; nothing else in the file is touched.
    fn try_resolve_package_json(&self, worktree_path: &Path, file: &str) -> Result<Option<String>> {
        let (Some(base), Some(ours), Some(theirs)) = (
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

        // Anything outside the dependency keys must be identical between
        // ours and theirs, or this resolver doesn't understand the
        // conflict well enough to touch it.
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
        Ok(Some(serde_json::to_string_pretty(&merged_value)? + "\n"))
    }

    /// Plain line-union merge for a `--union`-matched file: the result is
    /// "ours" lines, in order, followed by any of "theirs" lines not
    /// already present verbatim -- the same semantics as git's own
    /// `merge=union` attribute driver, just applied here in Rust rather
    /// than by mutating the repo's shared (cross-worktree)
    /// `.gitattributes`/config to register a driver. Appropriate only for
    /// genuinely order-independent, append-only content (barrel exports,
    /// changelog entries): the caller's own `--union <glob>` is what scopes
    /// this, pact does not try to guess which files qualify.
    fn try_resolve_union(&self, worktree_path: &Path, file: &str) -> Result<Option<String>> {
        let (Some(ours), Some(theirs)) = (
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
        Ok(Some(result))
    }
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

/// Builds a commit subject (and, for multi-line/long task text, a body) from
/// a workspace id and its task: `agent <id>: <first line of task>`, matching
/// the existing `pact/<id>` branch-naming convention so a commit is
/// traceable back to its workspace at a glance. The subject line is capped
/// around 72 chars (git convention); if the task is longer or spans
/// multiple lines, the full untruncated text follows in the commit body.
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

    // Full text goes in the body whenever the subject alone doesn't already
    // capture it losslessly -- either because there's more than one line,
    // or because the single line itself had to be truncated to fit.
    if task == first_line && !truncated {
        subject
    } else {
        format!("{subject}\n\n{task}")
    }
}

/// If `workspace` has a live agent process recorded, kills its whole
/// process tree before the worktree it's running in gets removed out from
/// under it, and waits (briefly) for it to actually be gone. Best-effort: a
/// dead/already-exited PID is silently ignored, not an error.
///
/// Killing only the tracked PID is not enough: confirmed directly, a
/// `claude` session running a Bash tool call spawns a child shell process,
/// and killing just the parent left that child alive, still holding a
/// handle into the workspace directory (as its own current_dir) for the
/// rest of its life -- which made every subsequent `git worktree remove`
/// and even a plain `remove_dir_all` fail with "used by another process."
/// On Windows, `taskkill /T` terminates the full descendant tree in one
/// call, which is what actually fixed it.
///
/// The Unix equivalent works because `pact-agents::run_and_stream` spawns
/// every agent process via `command_group`'s `group_spawn` (added for
/// issue #3's concurrent Ctrl-C handling), which calls `process_group(0)`
/// -- making the child its own process group leader, so its pgid equals
/// its pid. That means the *already-recorded* `agent_pid` (persisted to
/// disk, readable from a totally different `pact` process than the one
/// that spawned it) is sufficient on its own to kill the whole group:
/// `kill(-pid, SIGKILL)` targets every process in that group, descendants
/// included, without needing to persist a separate pgid. Implemented from
/// documented POSIX process-group semantics and command_group's own source
/// (see `pact-agents::supervisor`), but -- per issue #6 -- not yet
/// exercised on real Unix hardware, since this project's dev environment
/// is Windows-only; treat as implemented-not-live-verified until that
/// happens.
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
            // Negative PID targets the whole process group -- see the doc
            // comment above for why the group id and the recorded pid are
            // the same number here.
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    // Wait for the OS to actually reap it -- killing a process doesn't
    // mean its file handles (e.g. on its own current_dir) are released the
    // instant the kill call returns.
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
}
