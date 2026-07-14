//! Cross-platform integration test for issue #6: confirms that killing a
//! `Supervisor`-registered child actually reaches a grandchild process too,
//! on every platform CI runs on -- not just Windows, where this was
//! originally hand-verified only via `examples/group_kill_check.rs`. Runs
//! automatically under `cargo test --workspace` in CI (ubuntu-latest,
//! macos-latest, windows-latest), so the Unix path -- previously
//! implemented from documented POSIX semantics but never actually
//! exercised -- now gets a real, automated check on every push, without
//! needing real Unix hardware or any credentials.
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

    // Same call the Ctrl-C handler makes: reach into the registry and kill
    // every registered group. There's only one here.
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

/// A parent process that spawns a distinctly-named grandchild, so the test
/// can confirm the *grandchild* specifically died, not just the direct
/// child -- the exact gap the old plain `Child::kill()` path had (it only
/// killed the immediate process, not descendants a shell spawns).
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
    // `sh -c` is the parent (direct child of this test process); the `sleep
    // 60` it backgrounds and waits on is the grandchild whose survival
    // we're actually checking.
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
    // pgrep -f matches against the full command line, so this finds the
    // backgrounded `sleep 60` specifically, not unrelated `sleep` calls
    // elsewhere on a shared CI runner.
    let output = Command::new("pgrep")
        .args(["-f", "sleep 60"])
        .output()
        .expect("pgrep failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}
