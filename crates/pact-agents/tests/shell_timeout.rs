//! Issue #410: `run_shell_logged` bounds a verification command, kills its
//! whole tree at the timeout, and runs it the way CI would.

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("pact-agents-shell-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A background grandchild that writes `late.txt` after about three
/// seconds, while the shell itself waits much longer. Only a tree kill
/// stops the grandchild.
fn hang_with_a_late_grandchild() -> &'static str {
    if cfg!(windows) {
        "start /b cmd /c \"ping -n 4 127.0.0.1 >nul & echo late>late.txt\" & ping -n 60 127.0.0.1 >nul"
    } else {
        "(sleep 3; touch late.txt) & sleep 60"
    }
}

#[test]
fn a_command_past_its_timeout_is_killed_with_its_whole_tree() {
    let dir = scratch("timeout");
    let log = dir.join("out.log");
    let started = Instant::now();
    let run = pact_agents::run_shell_logged(hang_with_a_late_grandchild(), &dir, &log, Duration::from_secs(1)).unwrap();
    assert!(run.timed_out && run.status.is_none(), "{run:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "the wait ended at the timeout, took {:?}", started.elapsed());

    std::thread::sleep(Duration::from_secs(5));
    assert!(!dir.join("late.txt").exists(), "a grandchild outlived the timeout kill");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_command_runs_with_ci_set_closed_stdin_and_both_streams_in_one_log() {
    let dir = scratch("ci");
    let log = dir.join("out.log");
    let cmd = if cfg!(windows) {
        "echo out-line & echo err-line 1>&2 & if \"%CI%\"==\"true\" (exit 0) else (exit 1)"
    } else {
        "echo out-line; echo err-line >&2; cat; [ \"$CI\" = true ]"
    };
    let run = pact_agents::run_shell_logged(cmd, &dir, &log, Duration::from_secs(30)).unwrap();
    assert!(!run.timed_out, "{run:?}");
    assert!(run.status.is_some_and(|s| s.success()), "CI=true reaches the command: {run:?}");
    let text = std::fs::read_to_string(&log).unwrap();
    let out = text.find("out-line").unwrap_or_else(|| panic!("{text}"));
    let err = text.find("err-line").unwrap_or_else(|| panic!("{text}"));
    assert!(out < err, "the streams interleave in write order:\n{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
