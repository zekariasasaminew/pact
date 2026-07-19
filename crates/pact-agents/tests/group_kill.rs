//! Cross-platform integration test for issue #6 -- see DESIGN.md
//! ("pact-agents > Process group kill").
use std::process::{Command, Stdio};
use std::time::Duration;

use command_group::CommandGroup;
use pact_agents::Supervisor;

#[test]
fn killing_a_registered_group_reaches_a_grandchild_process() {
    let supervisor = Supervisor::new();

    let mut command = build_parent_command();
    command.stdout(Stdio::null()).stderr(Stdio::null());

    let child = command.group_spawn().expect("spawn failed");
    let pid = child.id();
    println!("spawned parent group, pid={pid}");
    let slot = supervisor.register(child);

    std::thread::sleep(Duration::from_secs(2));

    let before = count_marker_processes();
    assert!(
        before > 0,
        "expected the grandchild marker process to be running by now (found {before})"
    );

    let mut killed = supervisor.take(slot).expect("child was not registered");
    killed.kill().expect("group kill failed");
    println!("killed group {}", killed.id());

    std::thread::sleep(Duration::from_secs(2));

    let after = count_marker_processes();
    assert_eq!(
        after, 0,
        "grandchild marker process survived the group kill -- whole-tree kill did not work \
         (found {after} still running)"
    );
}

#[cfg(windows)]
fn build_parent_command() -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg(
        "ping -n 2 127.0.0.1 >NUL && ping -n 120 127.0.0.1 >NUL",
    );
    command
}

#[cfg(unix)]
fn build_parent_command() -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg("sleep 60 & wait");
    command
}

#[cfg(windows)]
fn count_marker_processes() -> usize {
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq ping.exe"])
        .output()
        .expect("tasklist failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.to_lowercase().contains("ping.exe"))
        .count()
}

#[cfg(unix)]
fn count_marker_processes() -> usize {
    let output = Command::new("pgrep")
        .args(["-f", "sleep 60"])
        .output()
        .expect("pgrep failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}
