//! Issue #237. Two things confirmed by real, adversarial tests here, not
//! just reasoned about -- see DESIGN.md ("pact-agents > Orphaned
//! grandchild cleanup, and a deeper pipe-inheritance root cause, issue
//! #237") for the full writeup:
//!
//! 1. `killing_a_process_group_after_its_primary_already_exited_still_
//!    reaches_a_grandchild` (fast, real regression coverage): the fix's
//!    actual mechanism -- calling `GroupChild::kill()` on a group whose
//!    tracked primary process has *already* exited still reaches and
//!    kills a grandchild that outlived it. This is the exact call
//!    `run_and_stream` now makes right after `wait()`.
//! 2. `run_and_stream_returns_promptly_despite_a_windows_grandchild_that_
//!    inherited_the_stdout_pipe` (Windows-only): a deeper root cause the
//!    investigation for this issue surfaced, and which issue #253 later
//!    fixed -- `run_and_stream` no longer blocks on stdout pipe EOF, so it
//!    returns promptly once the direct child exits regardless of what a
//!    grandchild does with an inherited handle.

use std::process::{Command, Stdio};
use std::time::Duration;

use command_group::CommandGroup;
use pact_agents::Supervisor;

#[cfg(windows)]
fn count_marker_processes() -> usize {
    let output = Command::new("tasklist").args(["/FI", "IMAGENAME eq ping.exe"]).output().expect("tasklist failed");
    String::from_utf8_lossy(&output.stdout).lines().filter(|l| l.to_lowercase().contains("ping.exe")).count()
}

#[cfg(unix)]
fn count_marker_processes() -> usize {
    let output = Command::new("pgrep").args(["-f", "sleep 120"]).output().expect("pgrep failed");
    String::from_utf8_lossy(&output.stdout).lines().filter(|l| !l.trim().is_empty()).count()
}

#[cfg(windows)]
fn build_parent_command() -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg("ping -n 2 127.0.0.1 >NUL && ping -n 120 127.0.0.1 >NUL");
    command
}

#[cfg(unix)]
fn build_parent_command() -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg("sleep 120 & wait");
    command
}

/// Real regression coverage for `run_and_stream`'s actual fix: `wait()`ing
/// for the tracked primary process to exit, then calling `kill()` on that
/// *same* `GroupChild` handle, must still reach and kill a grandchild
/// that's still alive -- the exact sequence `run_and_stream` performs
/// right after the direct agent process exits. Deliberately at this
/// lower level (not through `run_and_stream`'s stdout-piping), since that
/// path has its own, separate, deeper problem covered below.
#[test]
fn killing_a_process_group_after_its_primary_already_exited_still_reaches_a_grandchild() {
    let supervisor = Supervisor::new();
    let mut command = build_parent_command();
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let child = command.group_spawn().expect("spawn failed");
    let slot = supervisor.register(child);

    std::thread::sleep(Duration::from_secs(4));
    let before = count_marker_processes();
    assert!(before > 0, "expected the grandchild marker process to be running by now (found {before})");

    let mut group = supervisor.take(slot).expect("child was not registered");
    // The tracked primary (the outer cmd.exe/sh) is still running here
    // (it's the one still waiting on the long-lived ping/sleep) -- this
    // test intentionally doesn't call wait() first, since the point is
    // the same kill() call `run_and_stream` makes, which must work
    // whether or not the primary happens to have exited already.
    group.kill().expect("group kill failed");

    let mut remaining = count_marker_processes();
    for _ in 0..20 {
        if remaining == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        remaining = count_marker_processes();
    }
    assert_eq!(remaining, 0, "grandchild marker process survived the kill (found {remaining} still running)");
}

/// Real regression coverage for issue #253's fix. `fake_parent_with_
/// grandchild` (this package's own second `[[bin]]`) spawns a `ping -n 120`
/// grandchild with its own stdio explicitly set to `Stdio::null()`, then
/// exits almost immediately. On Windows, that grandchild still inherits
/// pact's own piped stdout write handle by default (`std::process::
/// Command`'s `bInheritHandles=true` duplicates *every* inheritable handle
/// open in the spawning process' table, not just the three explicitly
/// configured ones) -- so the pipe's write end stays open well past the
/// direct child's own exit. Before the fix, `run_and_stream` blocked
/// reading stdout until the grandchild's *own* ~119s lifetime ended --
/// pact's own process couldn't finish at all, matching the original
/// report's exact symptom (the shell never got its prompt back). The fix
/// races each stdout line against a poll of the direct child's exit status
/// instead of waiting for pipe EOF (see `run_and_stream`'s doc comment).
/// This test asserts both halves: `run_and_stream` returns promptly (not
/// after ~2 minutes), and the existing group-kill sweep still reaches and
/// kills the grandchild on the way out, so nothing is left orphaned.
#[test]
#[cfg(windows)]
fn run_and_stream_returns_promptly_despite_a_windows_grandchild_that_inherited_the_stdout_pipe() {
    let supervisor = Supervisor::new();
    let program = env!("CARGO_BIN_EXE_fake_parent_with_grandchild");
    let log_path = std::env::temp_dir().join(format!("pact-agents-orphan-cleanup-test-{}.log", std::process::id()));

    let start = std::time::Instant::now();
    let outcome = pact_agents::run_and_stream(
        &supervisor,
        program,
        &[],
        &std::env::temp_dir(),
        &log_path,
        |_line| Vec::new(),
        |_event| {},
        |_pid| {},
    );
    assert!(outcome.is_ok(), "run_and_stream must complete: {outcome:?}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "expected run_and_stream to return promptly once the direct child exits, not block on \
         the grandchild's own ~119s lifetime; took {:?}",
        start.elapsed()
    );

    let mut remaining = count_marker_processes();
    for _ in 0..20 {
        if remaining == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        remaining = count_marker_processes();
    }
    assert_eq!(remaining, 0, "grandchild marker process survived (found {remaining} still running)");

    let _ = std::fs::remove_file(&log_path);
}
