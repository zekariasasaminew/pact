//! Test-only helper for `tests/orphan_cleanup.rs` (issue #237): impersonates
//! an agent CLI that spawns its own long-lived sidecar (like a real agent's
//! MCP server subprocess) and exits immediately without waiting for it --
//! the exact "direct child gone, grandchild still running" shape a shell
//! one-liner proved unreliable to construct portably. The grandchild's own
//! stdio is explicitly null, not inherited, so it can never hold this
//! process's stdout pipe open -- this binary's own prompt exit is what lets
//! `run_and_stream`'s read loop see EOF quickly, matching a real agent CLI
//! that has already produced its last line of output.

use std::process::{Command, Stdio};

fn main() {
    let mut child = if cfg!(windows) {
        let mut c = Command::new("ping");
        c.args(["-n", "120", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("120");
        c
    };
    child.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = child.spawn();
    println!("{{\"type\":\"result\",\"success\":true,\"summary\":\"backgrounded a grandchild\"}}");
}
