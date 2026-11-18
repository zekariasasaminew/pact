//! Issue #366: every `Supervisor` in a process shares one registry, so the
//! single Ctrl-C handler reaches children from all of them, plus processes
//! registered by pid (the shared ACP agents). Its own test binary because
//! `kill_everything` acts on the whole process's registry.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use command_group::CommandGroup;
use pact_agents::{ChildPoll, Supervisor};

fn long_running() -> Command {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("ping");
        c.args(["-n", "60", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("60");
        c
    };
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command
}

#[test]
fn ctrl_c_reaches_children_of_every_supervisor_and_registered_pids() {
    let first = Supervisor::new();
    let second = Supervisor::new();
    let a = first.register(long_running().group_spawn().unwrap());
    let b = second.register(long_running().group_spawn().unwrap());
    let mut bare = long_running().spawn().unwrap();
    pact_agents::register_pid(bare.id());

    pact_agents::kill_everything();

    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = |supervisor: &Supervisor, slot| !matches!(supervisor.try_wait(slot), ChildPoll::Running);
    while Instant::now() < deadline && !(exited(&first, a) && exited(&second, b) && bare.try_wait().unwrap().is_some()) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(exited(&first, a), "the first supervisor's child survived");
    assert!(exited(&second, b), "the second supervisor's child survived");
    assert!(bare.try_wait().unwrap().is_some(), "the registered pid survived");
}
