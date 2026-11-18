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

#[test]
fn doctor_lists_dead_copilot_skill_directories_and_removes_them_on_request() {
    // Issue #304: pre-#294 test runs left registered temp paths behind.
    let home = std::env::temp_dir().join(format!("pact-doctor-copilot-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let alive = home.to_string_lossy().to_string();
    let dead = home.join("pact-cli-init-register-skill-gone").to_string_lossy().to_string();
    let settings = home.join("settings.json");
    std::fs::write(&settings, serde_json::json!({ "model": "x", "skillDirectories": [alive, dead] }).to_string()).unwrap();
    let doctor = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_pact")).arg("doctor").args(args).env("COPILOT_HOME", &home).output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let report = doctor(&[]);
    assert!(report.contains("lists skillDirectories that no longer exist") && report.contains(&dead) && !report.contains(&format!("  {alive}\n")), "{report}");
    let json: serde_json::Value = serde_json::from_str(&doctor(&["--json"])).unwrap();
    assert_eq!(json["copilot_dead_skill_directories"], serde_json::json!([dead]));
    assert!(std::fs::read_to_string(&settings).unwrap().contains("register-skill-gone"), "read-only without the flag");

    let fixed = doctor(&["--fix-skill-dirs"]);
    assert!(fixed.contains("removed 1 skillDirectories entry"), "{fixed}");
    let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after["skillDirectories"], serde_json::json!([alive]));
    assert_eq!(after["model"], "x", "other settings are kept");
    let _ = std::fs::remove_dir_all(&home);
}
