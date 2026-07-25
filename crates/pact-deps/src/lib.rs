//! The dependency broker (Phase 1). Detects a workspace's package
//! manager(s) and makes sure dependencies are ready before the agent's
//! first real command runs -- see DESIGN.md ("pact-deps") for the caching
//! strategy per ecosystem and the real Windows MAX_PATH failure that
//! shaped `prepare_npm`'s fallback path.

mod cmdutil;
mod detect;
mod passthrough;
mod store;

pub use cmdutil::run as run_shimmed;
pub use detect::{detect, PackageManager};
pub use store::{ContentStore, LinkMode};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One package manager's prep outcome -- see DESIGN.md ("pact-deps >
/// structured prep reporting", issue #12). Before this, `prepare` returned
/// bare `Result<()>` and every real failure was a `tracing::warn!` and
/// nothing else -- callers (and users) had no way to know which managers
/// were detected, which strategy ran, whether the npm content store was
/// hit or populated, or what materialization method was used, without
/// reading logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagerPrepReport {
    pub manager: String,
    pub strategy: String,
    pub store_key: Option<String>,
    pub store_hit: Option<bool>,
    pub materialization: Option<String>,
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
        store_key: None,
        store_hit: None,
        materialization: None,
        success,
        warnings,
    }
}

fn prepare_npm(workspace_path: &Path) -> ManagerPrepReport {
    let lockfile = workspace_path.join("package-lock.json");
    if !lockfile.exists() {
        let no_lockfile_note = format!(
            "no package-lock.json in {}; installing without sharing since there's nothing \
             stable to key a shared cache on, and with --no-package-lock so this workspace \
             doesn't generate its own lockfile (which would otherwise show up as a spurious \
             merge conflict against every other workspace that also has no lockfile)",
            workspace_path.display()
        );
        tracing::warn!("{no_lockfile_note}");
        // PackageManager::Npm intentionally bails in passthrough::run; fall
        // back to a plain, unshared install directly here instead.
        // `write_lockfile: false` -- see DESIGN.md ("pact-deps") for why.
        let mut warnings = vec![no_lockfile_note];
        let success = match passthrough::run(PackageManager::Npm, workspace_path)
            .or_else(|_| run_plain_npm_install(workspace_path, false))
        {
            Ok(success) => success,
            Err(err) => {
                warnings.push(format!("{err:#}"));
                false
            }
        };
        return ManagerPrepReport {
            manager: "npm".to_string(),
            strategy: "plain-install-no-lockfile".to_string(),
            store_key: None,
            store_hit: None,
            materialization: None,
            success,
            warnings,
        };
    }

    let key = match hash_file(&lockfile) {
        Ok(hash) => format!("{}-{hash}", platform_key()),
        Err(err) => {
            return ManagerPrepReport {
                manager: "npm".to_string(),
                strategy: "content-store".to_string(),
                store_key: None,
                store_hit: None,
                materialization: None,
                success: false,
                warnings: vec![format!("failed to hash {}: {err:#}", lockfile.display())],
            }
        }
    };
    let store = match store_root_for(workspace_path).and_then(ContentStore::new) {
        Ok(store) => store,
        Err(err) => {
            return ManagerPrepReport {
                manager: "npm".to_string(),
                strategy: "content-store".to_string(),
                store_key: Some(key),
                store_hit: None,
                materialization: None,
                success: false,
                warnings: vec![format!("failed to open the content store: {err:#}")],
            }
        }
    };

    // Checked *before* `populate_if_absent`, which would otherwise
    // populate it -- this is the only way to tell a cache hit from a
    // cache miss after the fact.
    let store_hit = store.entry_exists(&key);

    let populated = store.populate_if_absent(&key, |tmp| {
        std::fs::copy(workspace_path.join("package.json"), tmp.join("package.json"))
            .context("copying package.json into store staging dir")?;
        std::fs::copy(&lockfile, tmp.join("package-lock.json"))
            .context("copying package-lock.json into store staging dir")?;

        let output = cmdutil::run("npm", &["ci"], tmp)?;
        if !output.status.success() {
            anyhow::bail!("npm ci failed:\n{}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    });

    let entry = match populated {
        Ok(entry) => entry,
        Err(err) => {
            let note = format!(
                "populating the shared npm store failed for key '{key}', falling back to a \
                 normal (unshared) install for this workspace: {err:#}"
            );
            tracing::warn!("{note}");
            let mut warnings = vec![note];
            // A real lockfile exists here (this is the store-population
            // failure branch, past the no-lockfile early return above), so
            // this workspace's install should behave exactly as if pact
            // weren't involved at all: `npm install` may update the
            // existing lockfile in place, same as it would outside pact.
            let success = match run_plain_npm_install(workspace_path, true) {
                Ok(success) => success,
                Err(err) => {
                    warnings.push(format!("{err:#}"));
                    false
                }
            };
            return ManagerPrepReport {
                manager: "npm".to_string(),
                strategy: "plain-install-store-failed".to_string(),
                store_key: Some(key),
                store_hit: Some(store_hit),
                materialization: None,
                success,
                warnings,
            };
        }
    };

    let node_modules_src = entry.join("node_modules");
    let mut materialization = None;
    let mut warnings = Vec::new();
    let mut success = true;
    if node_modules_src.exists() {
        match ContentStore::materialize(&node_modules_src, &workspace_path.join("node_modules")) {
            Ok(mode) => materialization = Some(mode.as_str().to_string()),
            Err(err) => {
                warnings.push(format!("materializing node_modules failed: {err:#}"));
                success = false;
            }
        }
    }
    ManagerPrepReport {
        manager: "npm".to_string(),
        strategy: "content-store".to_string(),
        store_key: Some(key),
        store_hit: Some(store_hit),
        materialization,
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

/// Derives `.pact-<repo>/store/npm` from a workspace path of the form
/// `.pact-<repo>/workspaces/<id>`.
fn store_root_for(workspace_path: &Path) -> Result<PathBuf> {
    let state_dir = workspace_path
        .parent()
        .and_then(Path::parent)
        .context("could not derive state directory from workspace path")?;
    Ok(state_dir.join("store").join("npm"))
}

/// Distinguishes store entries by OS, architecture, libc flavor (Linux
/// only), Node major version, and npm's own version -- see DESIGN.md
/// ("pact-deps > Store key components") for why each dimension is there.
fn platform_key() -> String {
    let node_major = cmd_version_part("node", 0)
        .unwrap_or_else(|| "unknown".to_string());
    let npm_version = cmd_version_part("npm", -1)
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "{}-{}{}-node{}-npm{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        libc_suffix(),
        node_major,
        npm_version
    )
}

/// Runs `<program> --version` and returns either its first dot-separated
/// component (`part == 0`, e.g. Node's major version) or the whole trimmed
/// string (`part == -1`, e.g. npm's full version -- npm has no single
/// dominant compatibility axis the way Node's major version does, so the
/// full version is the more honest key component).
fn cmd_version_part(program: &str, part: i32) -> Option<String> {
    let output = cmdutil::run(program, &["--version"], Path::new(".")).ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let trimmed = raw.trim().trim_start_matches('v');
    if part == -1 {
        Some(trimmed.to_string())
    } else {
        trimmed.split('.').next().map(str::to_string)
    }
}

/// Empty on every platform except Linux, where it's `-musl` or `-glibc` --
/// see DESIGN.md ("pact-deps > Store key components").
fn libc_suffix() -> &'static str {
    if std::env::consts::OS != "linux" {
        return "";
    }
    let is_musl = std::fs::read_dir("/lib")
        .map(|entries| {
            entries.filter_map(Result::ok).any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("ld-musl-")
            })
        })
        .unwrap_or(false);
    if is_musl {
        "-musl"
    } else {
        "-glibc"
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_install_args_omits_lockfile_flag_when_writing_is_allowed() {
        assert_eq!(npm_install_args(true), vec!["install"]);
    }

    #[test]
    fn npm_install_args_adds_no_package_lock_flag_when_disallowed() {
        assert_eq!(npm_install_args(false), vec!["install", "--no-package-lock"]);
    }

    /// `store_root_for` derives the store root by walking two parents up
    /// from `workspace_path` (`state_dir/workspaces/<id>` -> `state_dir`),
    /// so every real test below nests its scratch workspace the same way
    /// a real one would be, not just a bare temp dir.
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
        assert!(report.store_key.is_none());
        assert!(!report.warnings.is_empty(), "expected a note about the missing lockfile");

        cleanup(&workspace);
    }

    #[test]
    fn prepare_npm_reports_a_content_store_hit_on_the_second_call() {
        let workspace = scratch_workspace("content-store-hit");
        std::fs::write(workspace.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
        std::fs::write(
            workspace.join("package-lock.json"),
            "{\"name\":\"scratch\",\"version\":\"1.0.0\",\"lockfileVersion\":3,\"packages\":{\"\":{\"name\":\"scratch\",\"version\":\"1.0.0\"}}}",
        )
        .unwrap();

        let first = prepare_npm(&workspace);
        assert_eq!(first.strategy, "content-store");
        assert_eq!(first.store_hit, Some(false), "first call must populate the store, not hit it");
        assert!(first.success, "warnings: {:?}", first.warnings);

        let second = prepare_npm(&workspace);
        assert_eq!(second.store_hit, Some(true), "second call with the same lockfile must hit the store");
        assert_eq!(second.store_key, first.store_key);

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
