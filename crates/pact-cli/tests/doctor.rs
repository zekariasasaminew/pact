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

#[test]
fn doctor_json_produces_valid_structured_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["doctor", "--json"])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();

    assert!(output.status.success(), "expected exit 0 with git present, stderr: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("expected valid JSON: {e}\ngot: {stdout}"));

    assert_eq!(report["git"]["found"], true);
    assert!(report["git"]["version"].is_string());
    assert_eq!(report["git"]["worktree_supported"], true);
    assert!(report["os"].is_string());
    assert!(report["arch"].is_string());

    let agents = report["agent_clis"].as_array().expect("agent_clis must be an array");
    assert!(!agents.is_empty());
    assert!(agents.iter().any(|a| a["name"] == "claude"));
    for agent in agents {
        assert!(agent["found"].is_boolean(), "got: {agent}");
    }

    let managers = report["package_managers"].as_array().expect("package_managers must be an array");
    assert!(managers.iter().any(|m| m["name"] == "npm"));
}

#[test]
fn doctor_json_and_human_readable_agree_on_git_worktree_support() {
    let json_output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["doctor", "--json"])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    let human_output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("doctor")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();

    let report: serde_json::Value = serde_json::from_slice(&json_output.stdout).unwrap();
    let human_stdout = String::from_utf8_lossy(&human_output.stdout);

    if report["git"]["worktree_supported"] == true {
        assert!(human_stdout.contains("worktree supported"), "got: {human_stdout}");
    } else {
        assert!(human_stdout.contains("too old for `git worktree`"), "got: {human_stdout}");
    }
}
