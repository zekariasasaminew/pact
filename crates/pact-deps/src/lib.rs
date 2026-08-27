//! The dependency broker (Phase 1). Detects a workspace's package
//! manager(s) and makes sure dependencies are ready before the agent's
//! first real command runs -- see DESIGN.md ("pact-deps") for the caching
//! strategy per ecosystem.
//!
//! npm relied on a custom lockfile-hash-keyed content store through issue
//! #233; that store is gone now, in favor of npm's own global cache
//! (`~/.npm` or wherever `npm config get cache` points), shared
//! automatically across concurrent `npm ci` calls with no pact-side
//! locking needed -- see DESIGN.md for why, and what was verified by hand
//! before deleting it.

mod cmdutil;
mod detect;
mod passthrough;

pub use cmdutil::run as run_shimmed;
pub use detect::{detect, PackageManager};

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// One package manager's prep outcome -- see DESIGN.md ("pact-deps >
/// structured prep reporting", issue #12). Before this, `prepare` returned
/// bare `Result<()>` and every real failure was a `tracing::warn!` and
/// nothing else -- callers (and users) had no way to know which managers
/// were detected, which strategy ran, or whether it succeeded, without
/// reading logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagerPrepReport {
    pub manager: String,
    pub strategy: String,
    pub success: bool,
    pub warnings: Vec<String>,
}

/// Prepares dependencies for every package manager detected in
/// `workspace_path`, returning one report per manager. Never fails the
/// caller for an individual ecosystem's install failure (captured in that
/// manager's own `success`/`warnings` instead) -- a workspace is still
/// usable, just possibly needing the agent to finish installing itself,
/// which is a slower path, not a broken one.
pub fn prepare(workspace_path: &Path) -> Vec<ManagerPrepReport> {
    detect::detect(workspace_path)
        .into_iter()
        .map(|manager| match manager {
            PackageManager::Npm => prepare_npm(workspace_path),
            other => prepare_passthrough(other, workspace_path),
        })
        .collect()
}

fn prepare_passthrough(manager: PackageManager, workspace_path: &Path) -> ManagerPrepReport {
    let (success, warnings) = match passthrough::run(manager, workspace_path) {
        Ok(success) => (success, Vec::new()),
        Err(err) => (false, vec![format!("{err:#}")]),
    };
    ManagerPrepReport {
        manager: manager.name().to_string(),
        strategy: "passthrough".to_string(),
        success,
        warnings,
    }
}

/// npm's own global cache (`~/.npm` or wherever `npm config get cache`
/// points) is shared automatically across every concurrent `npm ci` call
/// on the machine -- verified by hand under real concurrent load (5
/// workspaces racing a cold *and* a warm cache, no corruption, no errors)
/// before removing pact's own custom content store in favor of just
/// relying on it, issue #233. No pact-side key, lock, or materialization
/// step needed; `npm ci` is run directly in the workspace.
fn prepare_npm(workspace_path: &Path) -> ManagerPrepReport {
    let lockfile = workspace_path.join("package-lock.json");
    if !lockfile.exists() {
        let no_lockfile_note = format!(
            "no package-lock.json in {}; installing with --no-package-lock so this workspace \
             doesn't generate its own lockfile (which would otherwise show up as a spurious \
             merge conflict against every other workspace that also has no lockfile)",
            workspace_path.display()
        );
        tracing::warn!("{no_lockfile_note}");
        let mut warnings = vec![no_lockfile_note];
        let success = match run_plain_npm_install(workspace_path, false) {
            Ok(success) => success,
            Err(err) => {
                warnings.push(format!("{err:#}"));
                false
            }
        };
        return ManagerPrepReport {
            manager: "npm".to_string(),
            strategy: "plain-install-no-lockfile".to_string(),
            success,
            warnings,
        };
    }

    let (success, warnings) = match cmdutil::run("npm", &["ci"], workspace_path) {
        Ok(output) if output.status.success() => (true, Vec::new()),
        Ok(output) => (
            false,
            vec![format!("npm ci failed:\n{}", String::from_utf8_lossy(&output.stderr))],
        ),
        Err(err) => (false, vec![format!("{err:#}")]),
    };
    ManagerPrepReport {
        manager: "npm".to_string(),
        strategy: "npm-ci".to_string(),
        success,
        warnings,
    }
}

/// `write_lockfile: false` adds `--no-package-lock` so this install never
/// creates or updates `package-lock.json` in `workspace_path` -- used for
/// the no-committed-lockfile path, where a workspace-generated lockfile has
/// no stable content to converge on across workspaces (see issue #26).
/// `Ok(true)`/`Ok(false)` reflects the install's own exit code; `Err` means
/// it couldn't even be spawned.
fn run_plain_npm_install(workspace_path: &Path, write_lockfile: bool) -> Result<bool> {
    let args = npm_install_args(write_lockfile);
    let output = cmdutil::run("npm", &args, workspace_path)?;
    if !output.status.success() {
        tracing::warn!(
            "`npm {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(false);
    }
    Ok(true)
}

fn npm_install_args(write_lockfile: bool) -> Vec<&'static str> {
    let mut args = vec!["install"];
    if !write_lockfile {
        args.push("--no-package-lock");
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn npm_install_args_omits_lockfile_flag_when_writing_is_allowed() {
        assert_eq!(npm_install_args(true), vec!["install"]);
    }

    #[test]
    fn npm_install_args_adds_no_package_lock_flag_when_disallowed() {
        assert_eq!(npm_install_args(false), vec!["install", "--no-package-lock"]);
    }

    fn scratch_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pact-deps-test-{name}-{}", std::process::id())).join("workspaces").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(workspace_path: &Path) {
        // workspace_path is .../workspaces/<id> -- remove from state_dir up.
        if let Some(state_dir) = workspace_path.parent().and_then(Path::parent) {
            let _ = std::fs::remove_dir_all(state_dir);
        }
    }

    #[test]
    fn prepare_npm_reports_plain_install_when_no_lockfile_present() {
        let workspace = scratch_workspace("no-lockfile");
        std::fs::write(workspace.join("package.json"), "{}").unwrap();

        let report = prepare_npm(&workspace);
        assert_eq!(report.manager, "npm");
        assert_eq!(report.strategy, "plain-install-no-lockfile");
        assert!(!report.warnings.is_empty(), "expected a note about the missing lockfile");

        cleanup(&workspace);
    }

    #[test]
    fn prepare_npm_runs_npm_ci_when_a_lockfile_is_present() {
        let workspace = scratch_workspace("npm-ci");
        std::fs::write(workspace.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
        std::fs::write(
            workspace.join("package-lock.json"),
            "{\"name\":\"scratch\",\"version\":\"1.0.0\",\"lockfileVersion\":3,\"packages\":{\"\":{\"name\":\"scratch\",\"version\":\"1.0.0\"}}}",
        )
        .unwrap();

        let first = prepare_npm(&workspace);
        assert_eq!(first.strategy, "npm-ci");
        assert!(first.success, "warnings: {:?}", first.warnings);

        // Idempotent: relying on npm's own cache/lockfile-driven `npm ci`
        // means a second call in the same workspace must succeed the same
        // way, not just on a first, empty node_modules.
        let second = prepare_npm(&workspace);
        assert_eq!(second.strategy, "npm-ci");
        assert!(second.success, "warnings: {:?}", second.warnings);

        cleanup(&workspace);
    }

    #[test]
    fn prepare_passthrough_reports_success_for_a_real_available_manager() {
        // cargo is guaranteed present in this workspace's own build/test
        // environment -- `cargo fetch` against this real crate's own
        // Cargo.toml is fast and uses the already-warm registry index.
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let report = prepare_passthrough(PackageManager::Cargo, &workspace);
        assert_eq!(report.manager, "cargo");
        assert_eq!(report.strategy, "passthrough");
        assert!(report.success, "warnings: {:?}", report.warnings);
    }
}
