//! Per-task results (issue #389): `land`, `discard`, and the workspace
//! entries behind `pact list --json`. See DESIGN.md ("`pact land` and
//! `pact discard`").

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use pact_vcs::{agent_process_alive, LandKind, LandOutcome};
use serde::Serialize;

use crate::{fleet::WorkerReport, Orchestrator};

/// Where a workspace's agent run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    /// An agent process is alive in the workspace.
    Running,
    /// The last run exited successfully.
    Finished,
    /// The last run exited with a failure.
    Failed,
    /// No run has been recorded: the agent has not finished a run yet, or
    /// the workspace was created without one.
    Idle,
}

/// One workspace as `pact list --json` and the GUI see it.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceEntry {
    pub id: String,
    pub repo: PathBuf,
    pub branch: String,
    pub path: PathBuf,
    pub task: String,
    pub created_at: u64,
    pub state: EntryState,
    /// `None` when the worktree could not be inspected.
    pub dirty: Option<bool>,
    /// Commits on the branch that the repository's checked-out branch lacks.
    pub commits_ahead: Option<usize>,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Seconds the last run took.
    pub seconds: Option<u64>,
    pub summary: Option<String>,
    pub report: Option<WorkerReport>,
    pub shared_batch: Option<String>,
}

/// What `discard` threw away.
#[derive(Debug, Clone, Serialize)]
pub struct DiscardOutcome {
    pub id: String,
    pub branch: String,
    /// Files with uncommitted changes at the time.
    pub uncommitted_files: usize,
    /// Commits on the branch that the checked-out branch lacked.
    pub commits: usize,
}

/// First line of a task, cut to a pull-request title's length.
fn pr_title(task: &str) -> String {
    let first_line = task.trim().lines().next().unwrap_or_default().trim();
    if first_line.chars().count() > 72 {
        format!("{}...", first_line.chars().take(69).collect::<String>())
    } else {
        first_line.to_string()
    }
}

fn pr_body(task: &str, report: Option<&WorkerReport>) -> String {
    let mut body = format!("Task given to the agent:\n\n{}\n", task.trim());
    if let Some(report) = report {
        for (label, value) in [
            ("Changed", &report.changed),
            ("Verified", &report.verified),
            ("Not verified", &report.not_verified),
            ("Notes", &report.notes),
        ] {
            if !value.is_empty() {
                body.push_str(&format!("\n{label}: {value}\n"));
            }
        }
    }
    body
}

impl Orchestrator {
    fn refuse_while_running(&self, id: &str) -> Result<()> {
        let workspace = self.workspaces.get_workspace(id)?;
        if let Some(pid) = workspace.agent_pid.filter(|pid| agent_process_alive(*pid)) {
            bail!(
                "workspace {id}'s agent is still running (pid {pid}). Wait for it to finish, \
                 or stop it and drop its work with `pact discard {id}`."
            );
        }
        Ok(())
    }

    /// Lands one workspace: commits it, then either merges its branch into
    /// the repository's checked-out branch and removes the workspace, or
    /// with `pr` pushes the branch to `origin`, opens a pull request with
    /// `gh`, and removes only the worktree.
    pub fn land(&self, id: &str, pr: bool) -> Result<LandOutcome> {
        self.refuse_while_running(id)?;
        let outcome = if pr { self.land_as_pull_request(id)? } else { self.workspaces.land_workspace(id)? };
        self.log_operation(
            "land",
            Some(id),
            serde_json::json!({
                "target_branch": outcome.target_branch,
                "kind": outcome.kind,
                "commits": outcome.commits,
                "pr_url": outcome.pr_url,
            }),
        );
        Ok(outcome)
    }

    fn land_as_pull_request(&self, id: &str) -> Result<LandOutcome> {
        let workspace = self.workspaces.get_workspace(id)?;
        let report = self.run_metadata(id).and_then(|meta| meta.report);
        let mut outcome = self.workspaces.prepare_landing(id)?;
        if outcome.commits == 0 {
            bail!("workspace {id} has no changes to open a pull request for. Drop it with `pact discard {id}`.");
        }
        self.workspaces.push_branch(&outcome.branch, "origin")?;
        let created = Command::new("gh")
            .args(["pr", "create", "--base", &outcome.target_branch, "--head", &outcome.branch])
            .args(["--title", &pr_title(&workspace.task), "--body", &pr_body(&workspace.task, report.as_ref())])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to run `gh`; install the GitHub CLI and run `gh auth login`")?;
        if !created.status.success() {
            bail!(
                "`gh pr create` failed; branch {} is pushed and workspace {id} is left in place:\n{}",
                outcome.branch,
                String::from_utf8_lossy(&created.stderr)
            );
        }
        outcome.pr_url =
            String::from_utf8_lossy(&created.stdout).lines().rev().find(|l| l.contains("://")).map(|l| l.trim().to_string());
        outcome.kind = LandKind::PullRequest;
        self.workspaces.remove_workspace(id, true, false)?;
        Ok(outcome)
    }

    /// Removes a workspace and its branch, uncommitted and committed work
    /// included, stopping its agent first if one is running.
    pub fn discard(&self, id: &str) -> Result<DiscardOutcome> {
        let workspace = self.workspaces.get_workspace(id)?;
        let uncommitted_files =
            self.workspaces.workspace_diff(id).map(|diff| diff.uncommitted_status.lines().count()).unwrap_or(0);
        let commits = self.workspaces.commits_ahead(&workspace.branch).unwrap_or(0);
        self.teardown(id, false, true)?;
        Ok(DiscardOutcome { id: id.to_string(), branch: workspace.branch, uncommitted_files, commits })
    }

    /// The workspace's whole change as one unified diff, cut to
    /// `max_bytes`.
    pub fn patch(&self, id: &str, max_bytes: usize) -> Result<pact_vcs::WorkspacePatch> {
        self.workspaces.workspace_patch(id, max_bytes)
    }

    /// Every active workspace with its run state, for `pact list --json`
    /// and the GUI.
    pub fn entries(&self) -> Result<Vec<WorkspaceEntry>> {
        Ok(self
            .workspaces
            .list_workspaces()?
            .into_iter()
            .map(|workspace| {
                let meta = self.run_metadata(&workspace.id);
                let running = workspace.agent_pid.is_some_and(agent_process_alive);
                let state = match &meta {
                    _ if running => EntryState::Running,
                    Some(meta) if meta.exit_success => EntryState::Finished,
                    Some(_) => EntryState::Failed,
                    None => EntryState::Idle,
                };
                WorkspaceEntry {
                    dirty: self.workspaces.is_dirty(&workspace.id).ok(),
                    commits_ahead: self.workspaces.commits_ahead(&workspace.branch).ok(),
                    repo: self.repo_root.clone(),
                    state,
                    agent: meta.as_ref().map(|m| m.agent.clone()),
                    model: meta.as_ref().and_then(|m| m.model.clone()),
                    seconds: meta.as_ref().map(|m| m.ended_at.saturating_sub(m.started_at)),
                    summary: meta.as_ref().map(|m| m.summary.clone()),
                    report: meta.and_then(|m| m.report),
                    id: workspace.id,
                    branch: workspace.branch,
                    path: workspace.path,
                    task: workspace.task,
                    created_at: workspace.created_at,
                    shared_batch: workspace.shared_batch,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::ReportStatus;

    #[test]
    fn pr_title_is_the_first_line_of_the_task() {
        assert_eq!(pr_title("  Fix the login bug\n\nDetails follow."), "Fix the login bug");
    }

    #[test]
    fn pr_title_cuts_a_long_first_line_to_72_characters() {
        let title = pr_title(&"x".repeat(100));
        assert_eq!(title.chars().count(), 72);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn pr_body_carries_the_task_and_the_non_empty_report_fields() {
        let report = WorkerReport {
            status: ReportStatus::Done,
            changed: "a.rs".into(),
            verified: "cargo test".into(),
            not_verified: String::new(),
            notes: "none".into(),
        };
        let body = pr_body("Fix it", Some(&report));
        assert!(body.contains("Fix it"));
        assert!(body.contains("Changed: a.rs"));
        assert!(body.contains("Verified: cargo test"));
        assert!(!body.contains("Not verified"));
    }
}
