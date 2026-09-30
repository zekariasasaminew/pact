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
mod link;
mod passthrough;

pub use cmdutil::run as run_shimmed;
pub use detect::{detect, PackageManager};
pub use link::{ensure_git_ignores, link_dir, shareable_node_modules, NODE_MODULES};

use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// How dependency prep gets a workspace its dependencies -- see DESIGN.md
/// ("pact-deps > Link mode", issue #283). Measured on a 287-file Next.js
/// repo (Windows 11, NTFS, warm npm cache): a per-workspace `npm ci` took
/// 99 s and wrote 32,104 files; a junction to the repo root's existing
/// `node_modules` took 0.12 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DepsMode {
    /// `Link` when the repo root already has a `node_modules` to share,
    /// otherwise `Install`.
    #[default]
    Auto,
    /// Link `node_modules` to the repo root's install; every other
    /// ecosystem still gets its (cache-backed, cheap) passthrough install.
    /// Falls back to `Install` for the JavaScript manager, with a warning,
    /// if the repo root has nothing to share.
    Link,
    /// Run each detected manager's own install in the workspace.
    Install,
    /// Skip dependency prep entirely.
    None,
}

impl DepsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DepsMode::Auto => "auto",
            DepsMode::Link => "link",
            DepsMode::Install => "install",
            DepsMode::None => "none",
        }
    }
}

impl FromStr for DepsMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "auto" => Ok(DepsMode::Auto),
            "link" => Ok(DepsMode::Link),
            "install" => Ok(DepsMode::Install),
            "none" => Ok(DepsMode::None),
            other => Err(format!("unknown deps mode '{other}' (expected auto, link, install, or none)")),
        }
    }
}

impl std::fmt::Display for DepsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
    /// Workspace-relative paths this manager's prep created as links into
    /// a shared install rather than materializing (link mode, issue #283).
    /// Empty for every other strategy.
    #[serde(default)]
    pub linked_paths: Vec<String>,
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

/// `prepare`, with the strategy for JavaScript managers chosen by `mode`
/// -- see `DepsMode`. `repo_root` is where a shareable `node_modules` is
/// looked for. `DepsMode::None` returns no reports at all: "prep was never
/// attempted" is a different fact from "prep ran and found nothing to do".
pub fn prepare_with_mode(workspace_path: &Path, repo_root: &Path, mode: DepsMode) -> Vec<ManagerPrepReport> {
    let shareable = link::shareable_node_modules(repo_root);
    let effective = match mode {
        DepsMode::None => return Vec::new(),
        DepsMode::Install => DepsMode::Install,
        DepsMode::Link => DepsMode::Link,
        DepsMode::Auto if shareable.is_some() => DepsMode::Link,
        DepsMode::Auto => DepsMode::Install,
    };
    if effective == DepsMode::Install {
        return prepare(workspace_path);
    }

    detect::detect(workspace_path)
        .into_iter()
        .map(|manager| match (manager, &shareable) {
            (PackageManager::Npm | PackageManager::Pnpm | PackageManager::Yarn | PackageManager::Bun, Some(target)) => {
                prepare_link(manager, workspace_path, target)
            }
            (PackageManager::Npm | PackageManager::Pnpm | PackageManager::Yarn | PackageManager::Bun, None) => {
                let mut report = match manager {
                    PackageManager::Npm => prepare_npm(workspace_path),
                    other => prepare_passthrough(other, workspace_path),
                };
                report.warnings.insert(
                    0,
                    format!(
                        "link mode requested but {} has no {} to share; fell back to a real install",
                        repo_root.display(),
                        link::NODE_MODULES
                    ),
                );
                report
            }
            (other, _) => prepare_passthrough(other, workspace_path),
        })
        .collect()
}

fn prepare_link(manager: PackageManager, workspace_path: &Path, target: &Path) -> ManagerPrepReport {
    let link_path = workspace_path.join(link::NODE_MODULES);
    let (success, mut warnings, linked_paths) = match link::link_dir(target, &link_path) {
        Ok(()) => (true, Vec::new(), vec![link::NODE_MODULES.to_string()]),
        Err(err) => (false, vec![format!("{err:#}")], Vec::new()),
    };
    if success {
        if let Some(note) = link::ensure_git_ignores(workspace_path, link::NODE_MODULES) {
            warnings.push(note);
        }
    }
    ManagerPrepReport {
        manager: manager.name().to_string(),
        strategy: "link".to_string(),
        success,
        warnings,
        linked_paths,
    }
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
        linked_paths: Vec::new(),
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
            linked_paths: Vec::new(),
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
        linked_paths: Vec::new(),
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

    #[test]
    fn deps_mode_parses_every_name_and_rejects_unknown_ones() {
        assert_eq!("auto".parse::<DepsMode>(), Ok(DepsMode::Auto));
        assert_eq!("link".parse::<DepsMode>(), Ok(DepsMode::Link));
        assert_eq!("install".parse::<DepsMode>(), Ok(DepsMode::Install));
        assert_eq!("none".parse::<DepsMode>(), Ok(DepsMode::None));
        assert!("hardlink".parse::<DepsMode>().is_err());
        assert_eq!(DepsMode::default(), DepsMode::Auto);
        assert_eq!(DepsMode::Link.to_string(), "link");
    }

    fn scratch_repo_and_workspace(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("pact-deps-mode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let workspace = base.join("state").join("workspaces").join(name);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        for dir in [&repo, &workspace] {
            std::fs::write(dir.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
            std::fs::write(
                dir.join("package-lock.json"),
                "{\"name\":\"scratch\",\"version\":\"1.0.0\",\"lockfileVersion\":3,\"packages\":{\"\":{\"name\":\"scratch\",\"version\":\"1.0.0\"}}}",
            )
            .unwrap();
        }
        (repo, workspace)
    }

    fn cleanup_base(workspace: &Path) {
        // workspace is <base>/state/workspaces/<name>
        if let Some(base) = workspace.ancestors().nth(3) {
            let _ = std::fs::remove_dir_all(base);
        }
    }

    #[test]
    fn link_mode_links_node_modules_to_the_repo_root_install() {
        let (repo, workspace) = scratch_repo_and_workspace("link");
        std::fs::create_dir_all(repo.join(NODE_MODULES).join("left-pad")).unwrap();
        std::fs::write(repo.join(NODE_MODULES).join("left-pad").join("index.js"), "x").unwrap();

        let reports = prepare_with_mode(&workspace, &repo, DepsMode::Link);

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].strategy, "link");
        assert!(reports[0].success, "warnings: {:?}", reports[0].warnings);
        assert_eq!(reports[0].linked_paths, vec![NODE_MODULES.to_string()]);
        assert!(workspace.join(NODE_MODULES).join("left-pad").join("index.js").exists());
        cleanup_base(&workspace);
    }

    #[test]
    fn auto_mode_links_when_shareable_and_installs_otherwise() {
        let (repo, workspace) = scratch_repo_and_workspace("auto");
        let without = prepare_with_mode(&workspace, &repo, DepsMode::Auto);
        assert_eq!(without[0].strategy, "npm-ci", "no repo-root node_modules: auto must install");

        let _ = std::fs::remove_dir_all(workspace.join(NODE_MODULES));
        std::fs::create_dir_all(repo.join(NODE_MODULES)).unwrap();
        let with = prepare_with_mode(&workspace, &repo, DepsMode::Auto);
        assert_eq!(with[0].strategy, "link", "repo-root node_modules present: auto must link");
        cleanup_base(&workspace);
    }

    #[test]
    fn link_mode_without_a_shareable_install_falls_back_and_says_so() {
        let (repo, workspace) = scratch_repo_and_workspace("fallback");
        let reports = prepare_with_mode(&workspace, &repo, DepsMode::Link);
        assert_eq!(reports[0].strategy, "npm-ci");
        assert!(
            reports[0].warnings.first().is_some_and(|w| w.contains("fell back to a real install")),
            "warnings: {:?}",
            reports[0].warnings
        );
        assert!(reports[0].linked_paths.is_empty());
        cleanup_base(&workspace);
    }

    #[test]
    fn none_mode_produces_no_reports() {
        let (repo, workspace) = scratch_repo_and_workspace("none");
        assert!(prepare_with_mode(&workspace, &repo, DepsMode::None).is_empty());
        assert!(!workspace.join(NODE_MODULES).exists());
        cleanup_base(&workspace);
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
