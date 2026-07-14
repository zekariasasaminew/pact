//! Manual, Windows-only verification harness (not run by CI -- `cargo
//! build`/`test --workspace` don't build examples unless asked): confirms
//! that killing a `Supervisor`-registered child actually reaches a
//! grandchild process too (the exact gap the old plain `Child::kill()`
//! path had -- see `supervisor.rs`'s doc comment). Spawns
//! `cmd /C "ping ... && ping ..."`, where the second `ping` is a
//! grandchild of the `pact`-spawned `cmd.exe`, then kills the top-level
//! group and confirms every `ping.exe` under it is actually gone. Run with
//! `cargo run -p pact-agents --example group_kill_check`.
use std::process::{Command, Stdio};
use std::time::Duration;

use pact_agents::Supervisor;

fn main() {
    let supervisor = Supervisor::new();

    let mut command = Command::new("cmd");
    command
        .arg("/C")
        .arg("ping -n 3 127.0.0.1 >NUL && ping -n 120 127.0.0.1 >NUL")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = command_group::CommandGroup::group_spawn(&mut command).expect("spawn failed");
    let pid = child.id();
    println!("spawned cmd.exe group, pid={pid}");
    let _slot = supervisor.register(child);

    std::thread::sleep(Duration::from_secs(5));

    let ping_count_before = count_ping_processes();
    println!("ping.exe processes running before kill: {ping_count_before}");
    assert!(
        ping_count_before > 0,
        "expected the grandchild ping.exe to be running by now"
    );

    // Same call the Ctrl-C handler makes: reach into the registry and kill
    // every registered group. There's only one here.
    let killed = supervisor.take(_slot);
    match killed {
        Some(mut c) => {
            c.kill().expect("group kill failed");
            println!("killed group {}", c.id());
        }
        None => panic!("child was not registered"),
    }

    std::thread::sleep(Duration::from_secs(2));
    let ping_count_after = count_ping_processes();
    println!("ping.exe processes running after kill: {ping_count_after}");
    assert_eq!(
        ping_count_after, 0,
        "grandchild ping.exe survived the group kill -- whole-tree kill did not work"
    );

    println!("PASS: whole process group (including grandchild) was killed");
}

fn count_ping_processes() -> usize {
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq ping.exe"])
        .output()
        .expect("tasklist failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.to_lowercase().contains("ping.exe"))
        .count()
}
