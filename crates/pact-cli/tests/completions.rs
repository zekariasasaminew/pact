//! Integration coverage for issue #19: `pact completions <shell>` prints a
//! shell completion script generated via `clap_complete::generate`. Drives
//! the real built `pact` binary directly (clap-wiring behavior, same
//! reasoning as `version_flag.rs`) -- run from a directory that isn't a git
//! repo, since completions must work without a repo to operate on at all.
use std::process::Command;

fn run_completions(shell: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["completions", shell])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap()
}

#[test]
fn completions_bash_prints_a_script_without_requiring_a_git_repo() {
    let output = run_completions("bash");

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("_pact()"), "expected a bash completion function, got: {stdout}");
    assert!(stdout.contains("--agent"), "expected the generated script to mention a real flag like --agent, got: {stdout}");
}

#[test]
fn completions_supports_every_advertised_shell() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = run_completions(shell);
        assert!(
            output.status.success(),
            "pact completions {shell} failed: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "pact completions {shell} produced no output"
        );
    }
}
