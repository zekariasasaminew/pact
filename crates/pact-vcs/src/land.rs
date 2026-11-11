//! Landing one workspace on its own (issue #389): commit it, bring its
//! branch into the repository's checked-out branch, remove the worktree.
//! See DESIGN.md ("`pact land` and `pact discard`").

use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::{PidLock, WorkspaceManager, LOCK_TIMEOUT};

/// How a workspace's branch reached the target branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LandKind {
    FastForward,
    Merge,
    /// The branch held no commits the target lacked.
    NothingToLand,
    /// The branch was pushed and a pull request opened; the branch is kept.
    PullRequest,
}

#[derive(Debug, Clone, Serialize)]
pub struct LandOutcome {
    pub id: String,
    pub branch: String,
    pub target_branch: String,
    /// Whether landing had to commit uncommitted work first.
    pub committed: bool,
    /// Commits on the branch that the target lacked.
    pub commits: usize,
    pub kind: LandKind,
    pub pr_url: Option<String>,
}

impl WorkspaceManager {
    /// The branch checked out in the repository itself. Errors on a
    /// detached `HEAD`, where there is no branch to land onto.
    pub fn current_branch(&self) -> Result<String> {
        let output = Command::new("git")
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to spawn `git symbolic-ref`")?;
        if !output.status.success() {
            bail!(
                "{} has a detached HEAD, so there is no branch to land onto. Check out a branch first.",
                self.repo_root.display()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Commits on `branch` that the repository's `HEAD` lacks.
    pub fn commits_ahead(&self, branch: &str) -> Result<usize> {
        let output = Command::new("git")
            .args(["rev-list", "--count", &format!("HEAD..{branch}")])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to spawn `git rev-list`")?;
        if !output.status.success() {
            bail!("git rev-list failed for {branch}:\n{}", String::from_utf8_lossy(&output.stderr));
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .with_context(|| format!("unexpected `git rev-list --count` output for {branch}"))
    }

    /// Commits a workspace, refusing a member of a shared tree, and counts
    /// what its branch holds beyond the checked-out branch: the part
    /// `land_workspace` and a pull-request landing have in common. Nothing
    /// is merged or removed yet, so `kind` is `NothingToLand`.
    pub fn prepare_landing(&self, id: &str) -> Result<LandOutcome> {
        let workspace = self.get_workspace(id)?;
        if workspace.shared_batch.is_some() || !self.lanes_of(id)?.is_empty() {
            bail!(
                "workspace {id} is part of a shared tree, which holds several lanes' work in one worktree. \
                 Land the whole batch with `pact merge-all` instead."
            );
        }
        let target_branch = self.current_branch()?;
        let committed = self.commit_all(id)?;
        let commits = self.commits_ahead(&workspace.branch)?;
        Ok(LandOutcome {
            id: id.to_string(),
            branch: workspace.branch,
            target_branch,
            committed,
            commits,
            kind: LandKind::NothingToLand,
            pr_url: None,
        })
    }

    /// Commits the workspace, fast-forwards or merges its branch into the
    /// repository's checked-out branch, then removes the worktree and the
    /// branch. A conflict aborts the merge and leaves the workspace, now
    /// committed, in place.
    pub fn land_workspace(&self, id: &str) -> Result<LandOutcome> {
        let mut outcome = self.prepare_landing(id)?;
        if outcome.commits > 0 {
            let fast_forward = self.is_ancestor("HEAD", &outcome.branch)?;
            self.merge_into_checkout(id, &outcome.branch, &outcome.target_branch)?;
            outcome.kind = if fast_forward { LandKind::FastForward } else { LandKind::Merge };
        }
        self.remove_workspace(id, false, false)?;
        Ok(outcome)
    }

    fn merge_into_checkout(&self, id: &str, branch: &str, target_branch: &str) -> Result<()> {
        let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT).context("acquiring git worktree lock")?;
        let merge = Command::new("git")
            .args(["merge", "--no-edit", branch])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to spawn `git merge`")?;
        if merge.status.success() {
            return Ok(());
        }
        let conflicted = Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=U"])
            .current_dir(&self.repo_root)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        if conflicted.is_empty() {
            bail!(
                "could not merge {branch} into {target_branch}; workspace {id} is committed and left in place:\n{}{}",
                String::from_utf8_lossy(&merge.stdout),
                String::from_utf8_lossy(&merge.stderr)
            );
        }
        self.abort_merge(&self.repo_root, branch);
        bail!(
            "{branch} conflicts with {target_branch} in: {}. The merge was aborted and workspace {id} is \
             committed and left in place. Merge it by hand (`git merge {branch}`), open a pull request \
             (`pact land {id} --pr`), or drop it (`pact discard {id}`).",
            conflicted.join(", ")
        )
    }

    /// Pushes a workspace's branch to `remote` and sets it as upstream.
    pub fn push_branch(&self, branch: &str, remote: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["push", "--set-upstream", remote, branch])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to spawn `git push`")?;
        if !output.status.success() {
            bail!("git push {remote} {branch} failed:\n{}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }
}

/// A workspace's whole change as one unified diff, for the GUI's review
/// pane (issue #390).
#[derive(Debug, Clone, Serialize)]
pub struct WorkspacePatch {
    /// The commit `patch` is against: the merge base of the workspace's
    /// branch and the checked-out branch.
    pub base: String,
    pub patch: String,
    /// Whether `patch` was cut at the byte cap.
    pub truncated: bool,
}

impl WorkspaceManager {
    /// Everything the workspace holds beyond the commit it forked from,
    /// committed, uncommitted and untracked alike, as `git diff` text cut
    /// to `max_bytes`. The worktree's own index is left untouched: the
    /// files are staged into a throwaway copy of it.
    pub fn workspace_patch(&self, id: &str, max_bytes: usize) -> Result<WorkspacePatch> {
        let workspace = self.get_workspace(id)?;
        let base = git_stdout(&self.repo_root, &["merge-base", "HEAD", &workspace.branch])?;
        if base.is_empty() {
            bail!("workspace {id}'s branch shares no history with the checked-out branch, so there is no diff to show");
        }

        let scratch = self.state_dir().join("tmp");
        std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
        let index = scratch.join(format!("patch-index-{}-{id}", std::process::id()));
        let real_index = git_stdout(&workspace.path, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
        let _ = std::fs::copy(&real_index, &index);

        let staged = Command::new("git")
            .args(["add", "-A"])
            .env("GIT_INDEX_FILE", &index)
            .current_dir(&workspace.path)
            .output()
            .context("failed to spawn `git add`");
        let diff = staged.and_then(|staged| {
            if !staged.status.success() {
                bail!("git add failed in workspace {id}:\n{}", String::from_utf8_lossy(&staged.stderr));
            }
            Command::new("git")
                .args(["diff", "--cached", "--no-color", "--no-ext-diff", &base])
                .env("GIT_INDEX_FILE", &index)
                .current_dir(&workspace.path)
                .output()
                .context("failed to spawn `git diff`")
        });
        let _ = std::fs::remove_file(&index);
        let diff = diff?;
        if !diff.status.success() {
            bail!("git diff failed in workspace {id}:\n{}", String::from_utf8_lossy(&diff.stderr));
        }

        let mut patch = String::from_utf8_lossy(&diff.stdout).into_owned();
        let truncated = patch.len() > max_bytes;
        if truncated {
            let mut cut = max_bytes;
            while !patch.is_char_boundary(cut) {
                cut -= 1;
            }
            patch.truncate(patch[..cut].rfind('\n').map_or(cut, |newline| newline + 1));
        }
        Ok(WorkspacePatch { base, patch, truncated })
    }
}

fn git_stdout(dir: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
