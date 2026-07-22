//! Integration coverage for issue #18: `pact doctor` reports git, agent
//! CLI, and package-manager availability, and only exits non-zero when
//! something load-bearing (git) is missing. Drives the real built `pact`
//! binary directly, same reasoning as `completions.rs` -- doesn't need a
//! git repo to run in.
use std::process::Command;

#[test]
fn doctor_exits_0_when_git_is_present() {
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("doctor")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected exit 0 (git is present in the test environment), got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("git:"), "expected a git line, got: {stdout}");
    assert!(stdout.contains("agent CLIs:"), "expected an agent CLIs section, got: {stdout}");
    assert!(stdout.contains("package managers:"), "expected a package managers section, got: {stdout}");
}

#[test]
fn doctor_reports_a_missing_tool_as_not_found_not_an_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("doctor")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line.trim_start().starts_with("gradle:")),
        "expected a gradle line either way (found or not found), got: {stdout}"
    );
}
