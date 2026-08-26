//! Issue #238: a doc's own `pact ...` command examples must actually parse
//! against the real clap grammar. `SKILL.md` documented `pact diff --id
//! <id>` / `pact resolve --id <id>` / `pact teardown --id <id>` when the
//! real grammar is positional (`pact diff <id>`) -- caught by a human
//! following the docs literally, not by anything in CI. This makes that
//! whole class of bug structurally impossible going forward: every fenced
//! `pact`/`./pact` command line in README.md, SKILL.md, and
//! GETTING_STARTED.md is extracted and run against a real scratch repo,
//! asserting it isn't rejected as a clap usage error (exit code 2 is
//! clap's own convention for "arguments didn't parse" -- distinct from
//! `main`'s own runtime errors, which exit 1). See DESIGN.md ("pact-cli >
//! Doc/CLI grammar drift check (issue #238)").

use std::path::{Path, PathBuf};
use std::process::Command;

use uuid::Uuid;

/// Splits one logical shell line into argv-style tokens -- double-quoted
/// segments (may contain spaces/colons) stay one token, an unquoted `#`
/// starts a trailing comment and ends the line. Not a general shell
/// parser (no single-quote/escape handling) -- sufficient for this
/// project's own docs, which only ever use double quotes.
fn shell_split(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => break,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Extracts every `pact`/`./pact` command from fenced code blocks in
/// `markdown`, joining `\`-continued lines into one logical command
/// first. Returns each command's arguments (the program name itself
/// stripped).
fn extract_pact_commands(markdown: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut in_fence = false;
    let mut pending: Option<String> = None;

    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            continue;
        }

        let line = match pending.take() {
            Some(mut acc) => {
                acc.push(' ');
                acc.push_str(trimmed);
                acc
            }
            None => trimmed.to_string(),
        };

        if let Some(without_continuation) = line.strip_suffix('\\') {
            pending = Some(without_continuation.trim_end().to_string());
            continue;
        }

        let after_program = line.strip_prefix("pact ").or_else(|| line.strip_prefix("./pact "));
        let Some(rest) = after_program else { continue };

        commands.push(shell_split(rest));
    }
    commands
}

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn scratch_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-docs-grammar-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# scratch\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

/// Commands that would actually launch a real agent/spend money/hang if
/// run for real -- documented separately below and never invoked, since
/// this test's job is to check *argument parsing*, not to execute a real
/// `pact spawn`. `--dry-run` is used instead wherever a doc example
/// doesn't already have it, so the same argument grammar is exercised
/// without launching anything.
fn make_safe_to_run(mut args: Vec<String>) -> Vec<String> {
    let launches_an_agent = matches!(args.first().map(String::as_str), Some("spawn") | Some("spawn-many"));
    if launches_an_agent && !args.iter().any(|a| a == "--dry-run") {
        args.push("--dry-run".to_string());
    }
    args
}

fn assert_parses(repo: &Path, doc_name: &str, args: &[String]) {
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(make_safe_to_run(args.to_vec()))
        .current_dir(repo)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `pact {}`: {err}", args.join(" ")));
    assert_ne!(
        output.status.code(),
        Some(2),
        "{doc_name}: `pact {}` was rejected by clap as a usage error (exit 2) -- the doc's \
         command doesn't match the real CLI grammar:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn check_doc(repo: &Path, path: &str) {
    let markdown = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(path))
        .unwrap_or_else(|err| panic!("failed to read {path}: {err}"));
    let commands = extract_pact_commands(&markdown);
    assert!(!commands.is_empty(), "expected at least one `pact ...` command example in {path}");
    for args in &commands {
        assert_parses(repo, path, args);
    }
}

#[test]
fn readme_pact_commands_parse_against_the_real_cli() {
    let repo = scratch_repo();
    check_doc(&repo, "README.md");
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn skill_md_pact_commands_parse_against_the_real_cli() {
    let repo = scratch_repo();
    check_doc(&repo, "SKILL.md");
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn getting_started_pact_commands_parse_against_the_real_cli() {
    let repo = scratch_repo();
    check_doc(&repo, "GETTING_STARTED.md");
    let _ = std::fs::remove_dir_all(&repo);
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn shell_split_keeps_a_double_quoted_segment_as_one_token() {
        let tokens = shell_split(r#"spawn-many --task claude:"do a thing: with a colon""#);
        assert_eq!(tokens, vec!["spawn-many", "--task", "claude:do a thing: with a colon"]);
    }

    #[test]
    fn shell_split_drops_a_trailing_comment() {
        let tokens = shell_split("diff <id>   # some comment # with a hash");
        assert_eq!(tokens, vec!["diff", "<id>"]);
    }

    #[test]
    fn extract_pact_commands_joins_a_backslash_continued_command() {
        let markdown = "```sh\npact spawn-many \\\n  --task claude:\"a\" \\\n  --task claude:\"b\"\n```\n";
        let commands = extract_pact_commands(markdown);
        assert_eq!(commands, vec![vec!["spawn-many", "--task", "claude:a", "--task", "claude:b"]]);
    }

    #[test]
    fn extract_pact_commands_strips_the_relative_dot_slash_prefix() {
        let markdown = "```sh\n./pact spawn \"do the thing\"\n```\n";
        let commands = extract_pact_commands(markdown);
        assert_eq!(commands, vec![vec!["spawn", "do the thing"]]);
    }

    #[test]
    fn extract_pact_commands_ignores_non_pact_lines_and_outside_a_fence() {
        let markdown = "pact list\n```sh\n# a comment\ncurl https://example.com\npact list\n```\npact diff x\n";
        let commands = extract_pact_commands(markdown);
        assert_eq!(commands, vec![vec!["list"]]);
    }
}
