use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result};

/// Spawns `program` with `args` in `cwd`, routed through `cmd /C` on
/// Windows -- see DESIGN.md ("pact-deps > Windows .cmd shim resolution").
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
