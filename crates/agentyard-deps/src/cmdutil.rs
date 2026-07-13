use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result};

/// Spawns `program` with `args` in `cwd`.
///
/// On Windows, npm/pnpm/yarn (and sometimes poetry/pipenv, depending on
/// install method) ship as `.cmd` shims, not `.exe`. `std::process::Command`
/// does not consult `PATHEXT` the way a real shell does, so `Command::new("npm")`
/// fails with "program not found" even though `npm` works fine when typed
/// interactively. Routing through `cmd /C` restores that resolution; on
/// other platforms this is a plain, direct spawn.
pub fn run(program: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    let output = if cfg!(windows) {
        Command::new("cmd")
            .arg("/C")
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .output()
    } else {
        Command::new(program).args(args).current_dir(cwd).output()
    };
    output.with_context(|| format!("failed to spawn `{program} {}`", args.join(" ")))
}
