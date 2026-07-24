//! Integration coverage for issue #119: `pact demo` runs a real,
//! zero-cost, zero-decision walkthrough (real worktrees, real merge-all,
//! simulated agent output instead of a real paid agent call) and cleans
//! up after itself. Drives the real built `pact` binary; deliberately
//! run from a non-git temp directory to confirm the command doesn't
//! require being inside a git repo -- unlike every other pact command.
use std::process::Command;

#[test]
fn demo_succeeds_from_outside_any_git_repo() {
    let cwd = std::env::temp_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("demo")
        .current_dir(&cwd)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected `pact demo` to succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Simulating"), "expected simulated-agent framing, got: {stdout}");
    assert!(stdout.contains("running merge-all"), "expected a merge-all step, got: {stdout}");
    assert!(stdout.contains("merged"), "expected a successful merge in the report, got: {stdout}");
    assert!(stdout.contains("cleaning up the demo repo"), "expected explicit cleanup framing, got: {stdout}");
}

#[test]
fn demo_leaves_no_leftover_temp_directory() {
    // Reads the exact repo path *this* invocation printed and checks only
    // that one -- a generic "any pact-demo-* dir" sweep would be a false
    // failure whenever this test happens to run concurrently with
    // `demo_succeeds_from_outside_any_git_repo` in the same binary (cargo
    // runs tests in one binary in parallel by default), since the other
    // test's own still-running `pact demo` would show up in the sweep.
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("demo")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let repo_line = stdout
        .lines()
        .nth(1)
        .unwrap_or_else(|| panic!("expected a second line naming the demo repo path, got: {stdout}"));
    let repo_path = std::path::Path::new(repo_line.trim());

    assert!(
        !repo_path.exists(),
        "expected {} to be cleaned up after pact demo finished",
        repo_path.display()
    );
}
