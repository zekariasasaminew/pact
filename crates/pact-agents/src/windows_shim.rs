//! Resolves a Windows agent CLI's real, directly-executable target,
//! bypassing `cmd.exe` wherever possible -- see DESIGN.md ("pact-agents >
//! Windows multi-line prompt truncation", issue #210).
//!
//! `cmd.exe`'s own command-line reader treats a raw embedded newline as
//! ending the current line, no matter how the argument is quoted --
//! confirmed by hand, this truncates every flag after a multi-line `-p`
//! prompt value when an agent is spawned via the old `cmd /C <program>
//! <args>` wrapper. A genuine `.exe` target needs no shell at all (Windows'
//! own process creation resolves it directly); every agent CLI installed
//! via `npm install -g` (Copilot/Codex/Gemini, at least today) ships as a
//! `.cmd` shim following npm's own `cmd-shim` template, which this module
//! parses to find the real `node.exe` + script it ultimately execs, so
//! that can be spawned directly too. Anything that doesn't resolve either
//! way falls back to the existing `cmd /C` behavior unchanged.

use std::path::{Path, PathBuf};

/// What to spawn instead of going through `cmd.exe`: `program` plus any
/// argument that must come *before* the caller's own `args` (the resolved
/// script path, for a parsed `.cmd` shim; nothing extra for a plain `.exe`).
pub struct Resolved {
    pub program: PathBuf,
    pub leading_args: Vec<String>,
}

pub fn resolve(program: &str) -> Option<Resolved> {
    let path_var = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    resolve_in(program, &dirs)
}

/// Split out from `resolve` so tests can inject an explicit search-path
/// list instead of mutating the real, process-global `PATH` env var --
/// `std::env::set_var` isn't isolated between Rust's parallel test
/// threads, so a test that mutated it for real could flake against
/// unrelated tests reading `PATH` concurrently.
fn resolve_in(program: &str, dirs: &[PathBuf]) -> Option<Resolved> {
    if let Some(exe) = find_in(program, &["exe"], dirs) {
        return Some(Resolved { program: exe, leading_args: Vec::new() });
    }
    let shim = find_in(program, &["cmd", "bat"], dirs)?;
    let (interpreter, script) = parse_npm_cmd_shim(&shim)?;
    Some(Resolved { program: interpreter, leading_args: vec![script] })
}

fn find_in(program: &str, extensions: &[&str], dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        for ext in extensions {
            let candidate = dir.join(format!("{program}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// npm's `cmd-shim` package always ends its generated `.cmd` file with a
/// line of the shape `... & "%_prog%"  "%dp0%\<relative\script.js>" %*`,
/// where `%dp0%` is the shim file's own directory and `%_prog%` is that
/// same directory's sibling `node.exe` if present, else bare `node` from
/// PATH. Confirmed verbatim (only the relative script path differs)
/// against the real installed shims for `copilot`, `codex`, and `gemini`.
fn parse_npm_cmd_shim(shim_path: &Path) -> Option<(PathBuf, String)> {
    let contents = std::fs::read_to_string(shim_path).ok()?;
    let last_line = contents.lines().rev().find(|line| line.contains("%dp0%") && line.contains("%*"))?;

    let dp0_marker = "%dp0%";
    let after_dp0 = &last_line[last_line.find(dp0_marker)? + dp0_marker.len()..];
    let relative_script = &after_dp0[..after_dp0.find('"')?];
    let relative_script = relative_script.trim_start_matches(['\\', '/']);

    let shim_dir = shim_path.parent()?;
    let script_path = shim_dir.join(relative_script);
    if !script_path.is_file() {
        return None;
    }

    let sibling_node = shim_dir.join("node.exe");
    let interpreter = if sibling_node.is_file() { sibling_node } else { PathBuf::from("node") };
    Some((interpreter, script_path.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from the real installed `copilot.cmd`/`codex.cmd`/
    /// `gemini.cmd` shims -- only the final script path differs between
    /// them, confirmed by reading all three directly.
    const REAL_NPM_CMD_SHIM_TEMPLATE: &str = "@ECHO off\r\n\
GOTO start\r\n\
:find_dp0\r\n\
SET dp0=%~dp0\r\n\
EXIT /b\r\n\
:start\r\n\
SETLOCAL\r\n\
CALL :find_dp0\r\n\
\r\n\
IF EXIST \"%dp0%\\node.exe\" (\r\n\
  SET \"_prog=%dp0%\\node.exe\"\r\n\
) ELSE (\r\n\
  SET \"_prog=node\"\r\n\
  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
)\r\n\
\r\n\
endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@github\\copilot\\npm-loader.js\" %*\r\n";

    fn shim_fixture(dir_name: &str, script_relative: &str, with_sibling_node: bool) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pact-windows-shim-test-{dir_name}"));
        let script_path = root.join(script_relative);
        std::fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        std::fs::write(&script_path, "// fixture script").unwrap();
        if with_sibling_node {
            std::fs::write(root.join("node.exe"), "fixture node.exe").unwrap();
        }
        let shim_content =
            REAL_NPM_CMD_SHIM_TEMPLATE.replace("node_modules\\@github\\copilot\\npm-loader.js", script_relative);
        let shim_path = root.join("program.cmd");
        std::fs::write(&shim_path, shim_content).unwrap();
        shim_path
    }

    #[test]
    fn parses_the_real_npm_cmd_shim_template_with_a_sibling_node_exe() {
        let shim_path = shim_fixture("with-node", "node_modules\\@openai\\codex\\bin\\codex.js", true);
        let (interpreter, script) = parse_npm_cmd_shim(&shim_path).unwrap();
        assert_eq!(interpreter, shim_path.parent().unwrap().join("node.exe"));
        assert!(script.ends_with("codex.js"), "got: {script}");
        let _ = std::fs::remove_dir_all(shim_path.parent().unwrap());
    }

    #[test]
    fn falls_back_to_bare_node_when_no_sibling_node_exe_present() {
        let shim_path = shim_fixture("without-node", "node_modules\\@google\\gemini-cli\\bundle\\gemini.js", false);
        let (interpreter, script) = parse_npm_cmd_shim(&shim_path).unwrap();
        assert_eq!(interpreter, PathBuf::from("node"));
        assert!(script.ends_with("gemini.js"), "got: {script}");
        let _ = std::fs::remove_dir_all(shim_path.parent().unwrap());
    }

    #[test]
    fn returns_none_when_the_resolved_script_does_not_actually_exist() {
        let root = std::env::temp_dir().join("pact-windows-shim-test-missing-script");
        std::fs::create_dir_all(&root).unwrap();
        let shim_path = root.join("program.cmd");
        let shim_content = REAL_NPM_CMD_SHIM_TEMPLATE
            .replace("node_modules\\@github\\copilot\\npm-loader.js", "node_modules\\nonexistent\\script.js");
        std::fs::write(&shim_path, shim_content).unwrap();
        assert!(parse_npm_cmd_shim(&shim_path).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn returns_none_for_content_that_is_not_the_expected_shim_shape() {
        let root = std::env::temp_dir().join("pact-windows-shim-test-unrecognized");
        std::fs::create_dir_all(&root).unwrap();
        let shim_path = root.join("program.cmd");
        std::fs::write(&shim_path, "@echo off\r\nsome-other-tool.exe %*\r\n").unwrap();
        assert!(parse_npm_cmd_shim(&shim_path).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_in_locates_a_file_in_a_given_directory() {
        let dir = std::env::temp_dir().join("pact-windows-shim-test-find-in");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("myagent.exe"), "fixture").unwrap();

        let found = find_in("myagent", &["exe"], std::slice::from_ref(&dir));

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, Some(dir.join("myagent.exe")));
    }

    #[test]
    fn resolve_in_prefers_a_real_exe_over_a_cmd_shim_of_the_same_name() {
        let dir = std::env::temp_dir().join("pact-windows-shim-test-resolve-exe-priority");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("myagent.exe"), "fixture").unwrap();
        std::fs::write(dir.join("myagent.cmd"), "@echo off\r\n").unwrap();

        let resolved = resolve_in("myagent", std::slice::from_ref(&dir)).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(resolved.program, dir.join("myagent.exe"));
        assert!(resolved.leading_args.is_empty());
    }

    #[test]
    fn resolve_in_falls_back_to_a_parsed_cmd_shim_when_no_exe_exists() {
        let shim_path = shim_fixture("resolve-in-shim", "node_modules\\@github\\copilot\\npm-loader.js", true);
        let dir = shim_path.parent().unwrap().to_path_buf();
        // The fixture's shim file is named "program.cmd" -- resolve_in
        // looks for "<name>.cmd" so the search name must match.
        std::fs::rename(&shim_path, dir.join("myagent.cmd")).unwrap();

        let resolved = resolve_in("myagent", std::slice::from_ref(&dir)).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(resolved.program, dir.join("node.exe"));
        assert_eq!(resolved.leading_args.len(), 1);
        assert!(resolved.leading_args[0].ends_with("npm-loader.js"));
    }

    /// Manual verification against the 4 real installed agent CLIs, not a
    /// fixture -- `#[ignore]`d since CI's Windows runner doesn't have any
    /// of them installed. Confirmed by hand (issue #210): `claude` (native
    /// `.exe`) resolves with no leading args; `copilot`/`codex`/`gemini`
    /// (npm `.cmd` shims) each resolve to a real, existing `node.exe` +
    /// script path.
    #[test]
    #[ignore]
    fn resolves_all_4_real_installed_agent_clis_on_this_machine() {
        let exe_resolved = resolve("claude").expect("claude should resolve directly to claude.exe");
        assert!(exe_resolved.leading_args.is_empty(), "claude.exe needs no leading args");
        assert!(exe_resolved.program.is_file());

        for shimmed in ["copilot", "codex", "gemini"] {
            let resolved = resolve(shimmed).unwrap_or_else(|| panic!("{shimmed} should resolve via its .cmd shim"));
            assert_eq!(resolved.leading_args.len(), 1, "{shimmed}: expected exactly one leading arg (the script)");
            // Either a real sibling node.exe (absolute path, must exist),
            // or the bare "node" fallback (resolved via PATH at spawn
            // time, same as `Command::new("node")` would do -- not
            // itself a file path to check).
            assert!(
                resolved.program == Path::new("node") || resolved.program.is_file(),
                "{shimmed}: resolved interpreter {:?} should be 'node' or an existing file",
                resolved.program
            );
            assert!(
                Path::new(&resolved.leading_args[0]).is_file(),
                "{shimmed}: resolved script {:?} should exist",
                resolved.leading_args[0]
            );
        }
    }
}
