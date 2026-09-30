//! Stands in for a real agent CLI (`claude`/`copilot`/`codex`/`gemini`) in
//! end-to-end tests -- see DESIGN.md ("pact-cli > fake-agent end-to-end
//! harness", issue #157). Never spawns anything itself and never touches
//! the network; a real subprocess a test can shim onto `PATH` under a real
//! adapter's program name, so `pact spawn`/`spawn-many`/`merge-all` run
//! through their actual process-spawning, stdout-streaming, and
//! `AgentAdapter::parse_line` code paths instead of a stubbed-out closure.
//!
//! `pact` invokes an agent as `<program> -p <task> --output-format
//! stream-json ...` (Claude Code's own flag shape -- the only adapter this
//! binary impersonates for now, matching the "1-2 fake agents, not all four
//! adapters at once" v1 scope in issue #157). This binary reads the `-p`
//! value as a JSON `Script` (see below) instead of a natural-language
//! instruction, performs the scripted file writes relative to its current
//! directory (the real workspace worktree `run_and_stream` launches it in),
//! and prints Claude Code's real `stream-json` schema so
//! `claude_code::parse_line` parses it exactly as it would real output.

use std::io::Write;
use std::time::Duration;

use serde::Deserialize;

#[derive(Deserialize)]
struct Script {
    #[serde(default)]
    writes: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    sleep_ms: u64,
    #[serde(default = "default_summary")]
    summary: String,
    #[serde(default = "default_true")]
    success: bool,
    #[serde(default)]
    exit_code: i32,
    /// Environment variable names whose values (or `<unset>`) get written
    /// to `env-<NAME>.txt` in the working directory -- lets an end-to-end
    /// test prove what environment the launched agent actually saw
    /// (issue #284's per-agent `COPILOT_HOME`).
    #[serde(default)]
    dump_env: Vec<String>,
}

fn default_summary() -> String {
    "fake-agent: done".to_string()
}

fn default_true() -> bool {
    true
}

fn default_script() -> Script {
    Script {
        writes: std::collections::BTreeMap::new(),
        sleep_ms: 0,
        summary: default_summary(),
        success: true,
        exit_code: 0,
        dump_env: Vec::new(),
    }
}

/// Which real CLI this binary was copied onto `PATH` as -- decides which
/// output schema to print, since `pact` parses each adapter's real
/// format. Claude Code's `stream-json` unless the executable is named
/// `copilot`.
fn impersonates_copilot() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().eq_ignore_ascii_case("copilot")))
        .unwrap_or(false)
}

/// The `-p` value is the whole script for a direct `pact spawn`/`spawn-many`
/// call, but Arbiter wraps a workspace's original task text inside a larger
/// natural-language prompt -- so also try the first `{...}` substring found,
/// not just the whole string. Falls back to a no-op success if neither
/// parses, rather than failing the process outright.
fn parse_script(task: &str) -> Script {
    if let Ok(script) = serde_json::from_str(task) {
        return script;
    }
    if let (Some(start), Some(end)) = (task.find('{'), task.rfind('}')) {
        if end > start {
            if let Ok(script) = serde_json::from_str(&task[start..=end]) {
                return script;
            }
        }
    }
    default_script()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let task = args
        .iter()
        .position(|a| a == "-p")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_default();

    let script = parse_script(&task);
    let copilot = impersonates_copilot();

    if copilot {
        print_line(&serde_json::json!({
            "type": "session.mcp_servers_loaded",
            "data": {"servers": [{"name": "pact-coord", "status": "connected"}]},
        }));
    } else {
        print_line(&serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "fake-agent-session",
            "mcp_servers": [{"name": "pact-coord", "status": "connected"}],
        }));
    }

    if script.sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(script.sleep_ms));
    }

    for (path, content) in &script.writes {
        let dest = std::path::Path::new(path);
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let _ = std::fs::write(dest, content);
    }

    for name in &script.dump_env {
        let value = std::env::var(name).unwrap_or_else(|_| "<unset>".to_string());
        let _ = std::fs::write(format!("env-{name}.txt"), value);
    }

    if copilot {
        print_line(&serde_json::json!({
            "type": "result",
            "exitCode": if script.success { 0 } else { 1 },
        }));
    } else {
        print_line(&serde_json::json!({
            "type": "result",
            "is_error": !script.success,
            "result": script.summary,
        }));
    }

    std::process::exit(script.exit_code);
}

fn print_line(value: &serde_json::Value) {
    println!("{value}");
    let _ = std::io::stdout().flush();
}
