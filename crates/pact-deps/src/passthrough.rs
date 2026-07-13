use std::path::Path;

use anyhow::Result;

use crate::cmdutil;
use crate::detect::PackageManager;

/// Runs the normal install/fetch command for ecosystems that already have
/// a good global shared cache -- pnpm, yarn, uv, poetry, pipenv, Cargo, and
/// Go modules all cache once and reuse across projects by default, so the
/// only job here is making sure that cache gets warmed before the agent's
/// first real command, not building a new sharing mechanism. Maven and
/// Gradle need no command at all: `~/.m2` and `~/.gradle/caches` populate
/// lazily on any build invocation, so an explicit fetch step would only add
/// time for no benefit.
///
/// A non-zero exit is logged as a warning, not returned as an error --
/// a transient network failure here shouldn't fail the whole `spawn`; the
/// agent can still retry the install itself once it starts working.
pub fn run(manager: PackageManager, workspace_path: &Path) -> Result<()> {
    let (program, args): (&str, &[&str]) = match manager {
        PackageManager::Pnpm => ("pnpm", &["install", "--prefer-offline"]),
        PackageManager::Yarn => ("yarn", &["install", "--prefer-offline"]),
        PackageManager::Uv => ("uv", &["sync"]),
        PackageManager::Poetry => ("poetry", &["install"]),
        PackageManager::Pipenv => ("pipenv", &["install"]),
        PackageManager::Cargo => ("cargo", &["fetch"]),
        PackageManager::GoModules => ("go", &["mod", "download"]),
        PackageManager::Maven | PackageManager::Gradle => return Ok(()),
        PackageManager::PipPlain => return run_pip_plain(workspace_path),
        PackageManager::Npm => {
            anyhow::bail!("npm goes through the content store, not passthrough::run")
        }
    };

    run_command(program, args, workspace_path)
}

/// No custom store for plain pip/venv (Phase 1 decision, see README): pip
/// already has its own global download cache (`~/.cache/pip`) shared
/// across projects by default, covering the expensive part (network
/// fetch). Building a hardlink-based store on top of that would mean
/// hardlinking into freshly created venvs, which risks embedding absolute
/// paths from the wrong venv (activation scripts, `.pth` files, console
/// script shebangs) -- a correctness risk, not just extra engineering, so
/// it's left as future work rather than shipped provisionally.
fn run_pip_plain(workspace_path: &Path) -> Result<()> {
    if workspace_path.join("requirements.txt").exists() {
        run_command("pip", &["install", "-r", "requirements.txt"], workspace_path)
    } else {
        run_command("pip", &["install", "-e", "."], workspace_path)
    }
}

fn run_command(program: &str, args: &[&str], workspace_path: &Path) -> Result<()> {
    let output = cmdutil::run(program, args, workspace_path)?;

    if !output.status.success() {
        tracing::warn!(
            "`{program} {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
