//! `pact fleet` (issue #388): independent tasks, each in its own
//! repository if needed, one workspace and branch per task, never merged
//! together. See DESIGN.md ("pact-core > `pact fleet`").

use std::collections::BTreeMap;
use std::path::PathBuf;

use pact_agents::{AgentEvent, AgentKind, Supervisor};
use pact_vcs::Workspace;

use crate::{Admission, LaneRuntime, Orchestrator, SpawnOptions};

/// One independent task of a fleet.
pub struct FleetTask {
    /// Workspace name; the branch is `pact/<slug of name>`.
    pub name: String,
    /// Top level of the git repository the task runs in.
    pub repo: PathBuf,
    pub agent: AgentKind,
    /// `None` keeps the agent CLI's own default model.
    pub model: Option<String>,
    pub prompt: String,
}

/// How a worker says its task ended, in the report it closes with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportStatus {
    Done,
    Blocked,
    Failed,
}

/// The report a worker ends its final message with when its prompt
/// carried `REPORT_CONTRACT`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerReport {
    pub status: ReportStatus,
    pub changed: String,
    pub verified: String,
    pub not_verified: String,
    pub notes: String,
}

/// Appended to a prompt by `SpawnOptions::report_contract`.
pub const REPORT_CONTRACT: &str = "When you finish, end your final message with this report, one field per line:
STATUS: done | blocked | failed
CHANGED: <files you changed, comma separated, or none>
VERIFIED: <what you ran or checked that proves it works>
NOT VERIFIED: <what you could not check>
NOTES: <anything the reviewer must know; for blocked, the question>";

/// `prompt` followed by a blank line and the report contract.
pub fn with_report_contract(prompt: &str) -> String {
    format!("{}\n\n{REPORT_CONTRACT}", prompt.trim_end())
}

/// Strips the markdown a model tends to wrap a field label in (`- `,
/// `**STATUS:**`) and returns the label upper-cased with its value.
fn report_field(line: &str) -> Option<(String, String)> {
    let line = line.trim().trim_start_matches(['-', '*', '>', ' ', '`', '#']);
    let (label, value) = line.split_once(':')?;
    let label = label.trim().trim_matches(['*', '_', '`']).trim().to_ascii_uppercase();
    let value = value.trim().trim_start_matches(['*', '_']).trim().trim_end_matches('`').trim();
    Some((label, value.to_string()))
}

/// Reads the worker report out of an agent's final message. The last
/// `STATUS:` line wins, so a report quoted earlier in the message (the
/// contract itself, say) is ignored; the other fields are read from the
/// lines after it. `None` when there is no `STATUS:` line with a known
/// value.
pub fn parse_worker_report(text: &str) -> Option<WorkerReport> {
    let fields: Vec<(String, String)> = text.lines().filter_map(report_field).collect();
    // The contract's own template line reads "done | blocked | failed";
    // an agent that echoes it has not reported anything.
    let start = fields.iter().rposition(|(label, value)| label == "STATUS" && !value.contains('|'))?;
    let status_word: String =
        fields[start].1.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
    let status = match status_word.as_str() {
        "done" => ReportStatus::Done,
        "blocked" => ReportStatus::Blocked,
        "failed" => ReportStatus::Failed,
        _ => return None,
    };
    let field = |name: &str| {
        fields[start + 1..].iter().find(|(label, _)| label == name).map(|(_, value)| value.clone()).unwrap_or_default()
    };
    Some(WorkerReport {
        status,
        changed: field("CHANGED"),
        verified: field("VERIFIED"),
        not_verified: field("NOT VERIFIED"),
        notes: field("NOTES"),
    })
}

/// What became of one fleet task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetStatus {
    /// The agent process succeeded and reported `STATUS: done`.
    Done,
    Blocked,
    Failed,
    /// The agent finished without the report the contract asks for.
    Unreported,
    /// The task never produced an agent result: the repository could not
    /// be opened, the workspace could not be created, or the launch failed.
    Error,
}

impl FleetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Unreported => "unreported",
            Self::Error => "error",
        }
    }
}

/// One task's result, in the order the tasks were given.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FleetOutcome {
    pub index: usize,
    pub name: String,
    pub status: FleetStatus,
    /// `None` only when the workspace was never created.
    pub workspace: Option<Workspace>,
    pub report: Option<WorkerReport>,
    /// The agent's final message, or the error for `FleetStatus::Error`.
    pub summary: String,
    /// Seconds the agent ran, not counting time queued for admission.
    pub seconds: u64,
}

fn status_of(process_succeeded: bool, report: Option<&WorkerReport>) -> FleetStatus {
    match report.map(|r| r.status) {
        None => FleetStatus::Unreported,
        Some(ReportStatus::Blocked) => FleetStatus::Blocked,
        Some(ReportStatus::Done) if process_succeeded => FleetStatus::Done,
        Some(ReportStatus::Done | ReportStatus::Failed) => FleetStatus::Failed,
    }
}

/// Runs every task concurrently as a process lane in a workspace of its
/// own, in its own repository, under one `Supervisor` (one Ctrl-C stops
/// them all) and one `Admission` (the concurrency limit and memory floor
/// in `options.admission` cover the whole fleet, not each repository).
/// `on_event` receives the task's index with each event and is called
/// from that task's thread.
pub fn run_fleet(
    tasks: Vec<FleetTask>,
    options: &SpawnOptions<'_>,
    on_event: impl Fn(usize, &AgentEvent) + Sync,
) -> Vec<FleetOutcome> {
    let supervisor = Supervisor::new();
    let admission = Admission::new(options.admission);

    let mut orchestrators: BTreeMap<PathBuf, Result<Orchestrator, String>> = BTreeMap::new();
    for task in &tasks {
        orchestrators
            .entry(task.repo.clone())
            .or_insert_with(|| Orchestrator::open(task.repo.clone()).map_err(|err| format!("{err:#}")));
    }

    std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                let supervisor = &supervisor;
                let admission = &admission;
                let on_event = &on_event;
                let orchestrator = &orchestrators[&task.repo];
                scope.spawn(move || run_task(index, task, orchestrator, options, supervisor, admission, on_event))
            })
            .collect();

        handles
            .into_iter()
            .zip(&tasks)
            .enumerate()
            .map(|(index, (handle, task))| {
                handle.join().unwrap_or_else(|panic| {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "no panic message".to_string());
                    error_outcome(index, task, format!("fleet task thread panicked: {message}"))
                })
            })
            .collect()
    })
}

fn error_outcome(index: usize, task: &FleetTask, summary: String) -> FleetOutcome {
    FleetOutcome {
        index,
        name: task.name.clone(),
        status: FleetStatus::Error,
        workspace: None,
        report: None,
        summary,
        seconds: 0,
    }
}

fn run_task(
    index: usize,
    task: &FleetTask,
    orchestrator: &Result<Orchestrator, String>,
    options: &SpawnOptions<'_>,
    supervisor: &Supervisor,
    admission: &Admission,
    on_event: &(impl Fn(usize, &AgentEvent) + Sync),
) -> FleetOutcome {
    let orchestrator = match orchestrator {
        Ok(orchestrator) => orchestrator,
        Err(err) => return error_outcome(index, task, format!("could not open {}: {err}", task.repo.display())),
    };
    let lane_options = SpawnOptions {
        model: task.model.as_deref(),
        runtime: LaneRuntime::Process,
        shared_tree: false,
        report_contract: true,
        ..*options
    };
    let result = orchestrator.spawn_with_supervisor(
        supervisor,
        task.agent,
        &task.prompt,
        Some(&task.name),
        &lane_options,
        Some(admission),
        None,
        None,
        |event| on_event(index, event),
    );
    match result {
        Ok((workspace, run)) => {
            let report = parse_worker_report(&run.summary);
            let seconds = orchestrator
                .run_metadata(&workspace.id)
                .map(|meta| meta.ended_at.saturating_sub(meta.started_at))
                .unwrap_or(0);
            FleetOutcome {
                index,
                name: task.name.clone(),
                status: status_of(run.success, report.as_ref()),
                workspace: Some(workspace),
                report,
                summary: run.summary,
                seconds,
            }
        }
        Err(err) => error_outcome(index, task, format!("{err:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_worker_report_reads_every_field() {
        let report = parse_worker_report(
            "All set.\n\nSTATUS: done\nCHANGED: src/a.rs, src/b.rs\nVERIFIED: cargo test\nNOT VERIFIED: the Windows path\nNOTES: none",
        )
        .unwrap();
        assert_eq!(report.status, ReportStatus::Done);
        assert_eq!(report.changed, "src/a.rs, src/b.rs");
        assert_eq!(report.verified, "cargo test");
        assert_eq!(report.not_verified, "the Windows path");
        assert_eq!(report.notes, "none");
    }

    #[test]
    fn parse_worker_report_tolerates_markdown_bold_and_list_markers() {
        let report =
            parse_worker_report("- **STATUS:** blocked\n- **NOTES:** which database should this use?").unwrap();
        assert_eq!(report.status, ReportStatus::Blocked);
        assert_eq!(report.notes, "which database should this use?");
        assert_eq!(report.changed, "");

        let report = parse_worker_report("**Status**: Failed.\n**Notes**: the build is broken on main").unwrap();
        assert_eq!(report.status, ReportStatus::Failed);
        assert_eq!(report.notes, "the build is broken on main");
    }

    #[test]
    fn parse_worker_report_uses_the_last_status_line() {
        let report = parse_worker_report(
            "STATUS: blocked\nNOTES: first attempt\nThen I found the answer.\nSTATUS: done\nNOTES: second attempt",
        )
        .unwrap();
        assert_eq!(report.status, ReportStatus::Done);
        assert_eq!(report.notes, "second attempt");
    }

    #[test]
    fn parse_worker_report_is_none_without_a_usable_status() {
        assert_eq!(parse_worker_report("I changed the file and it works."), None);
        assert_eq!(parse_worker_report("STATUS: probably fine"), None);
        assert_eq!(parse_worker_report(REPORT_CONTRACT), None);
    }

    #[test]
    fn with_report_contract_appends_after_one_blank_line() {
        let prompt = with_report_contract("Fix the login bug.\n");
        assert!(prompt.starts_with("Fix the login bug.\n\nWhen you finish"));
        assert!(prompt.ends_with("for blocked, the question>"));
    }

    #[test]
    fn a_task_is_done_only_when_the_process_succeeded_and_reported_done() {
        let report = |status| WorkerReport {
            status,
            changed: String::new(),
            verified: String::new(),
            not_verified: String::new(),
            notes: String::new(),
        };
        assert_eq!(status_of(true, Some(&report(ReportStatus::Done))), FleetStatus::Done);
        assert_eq!(status_of(false, Some(&report(ReportStatus::Done))), FleetStatus::Failed);
        assert_eq!(status_of(true, Some(&report(ReportStatus::Blocked))), FleetStatus::Blocked);
        assert_eq!(status_of(true, Some(&report(ReportStatus::Failed))), FleetStatus::Failed);
        assert_eq!(status_of(true, None), FleetStatus::Unreported);
    }
}
