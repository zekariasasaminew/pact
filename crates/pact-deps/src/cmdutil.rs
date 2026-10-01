use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result};

/// Spawns `program` with `args` in `cwd`, routed through `cmd /C` on
/// Windows -- see DESIGN.md ("pact-deps > Windows .cmd shim resolution").
pub fn run(program: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    build_command(program, args, cwd).output().with_context(|| format!("failed to spawn `{program} {}`", args.join(" ")))
}

/// Like [`run`], but for read-only *detection* probes (e.g. `pnpm
/// --version` in `pact doctor`): runs in a neutral temp directory with
/// Corepack's package.json auto-pin disabled, so a probe never mutates a
/// project it happens to be run from -- see DESIGN.md ("pact-deps >
/// Detection probes must not write to the project", issue #299). On
/// Corepack-managed shims (`pnpm`, `yarn`) a bare `--version` call
/// otherwise appends a `packageManager` field to the nearest package.json.
pub fn run_probe(program: &str, args: &[&str]) -> Result<Output> {
    probe_command(program, args).output().with_context(|| format!("failed to spawn `{program} {}`", args.join(" ")))
}

fn probe_command(program: &str, args: &[&str]) -> Command {
    let mut command = build_command(program, args, &std::env::temp_dir());
    command.env("COREPACK_ENABLE_AUTO_PIN", "0");
    command
}

fn build_command(program: &str, args: &[&str], cwd: &Path) -> Command {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    command.current_dir(cwd);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_runs_in_the_temp_dir_not_the_current_project() {
        let command = probe_command("pnpm", &["--version"]);
        assert_eq!(command.get_current_dir(), Some(std::env::temp_dir().as_path()));
    }

    #[test]
    fn probe_disables_corepack_auto_pin() {
        // Corepack appends a `packageManager` field to the nearest package.json on any
        // pnpm/yarn invocation unless this is set -- issue #299.
        let command = probe_command("pnpm", &["--version"]);
        let pin = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("COREPACK_ENABLE_AUTO_PIN"))
            .and_then(|(_, value)| value);
        assert_eq!(pin, Some(std::ffi::OsStr::new("0")));
    }
}
