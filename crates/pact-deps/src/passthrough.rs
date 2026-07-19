use std::path::Path;

use anyhow::Result;

use crate::cmdutil;
use crate::detect::PackageManager;

/// Warms the package manager's own global cache for ecosystems that already
/// have one, instead of building pact-specific sharing -- see DESIGN.md
/// ("pact-deps > Passthrough caching strategy"). Failures are logged, not
/// returned as an error.
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

/// No custom store for plain pip/venv -- see DESIGN.md ("pact-deps >
/// Passthrough caching strategy").
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
