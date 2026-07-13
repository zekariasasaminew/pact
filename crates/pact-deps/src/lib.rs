//! The dependency broker (Phase 1).
//!
//! Detects a workspace's package manager(s) and makes sure dependencies are
//! ready before the agent's first real command runs. Most ecosystems
//! (pnpm, yarn, uv, poetry, pipenv, Cargo, Go modules, Maven, Gradle)
//! already have a good global shared cache, so those just get their normal
//! install/fetch command run (see `passthrough`). npm (flat, per-project
//! `node_modules`, no built-in sharing) is routed through a lockfile-hash-
//! keyed content store instead (see `store`), materialized via reflink or
//! read-only hardlink so a second+ workspace doesn't pay for a full
//! reinstall. Plain pip/venv is intentionally left as passthrough-only --
//! see `passthrough::run_pip_plain` for why a custom store there was
//! rejected rather than deferred by accident.

mod cmdutil;
mod detect;
mod passthrough;
mod store;

pub use detect::{detect, PackageManager};
pub use store::{ContentStore, LinkMode};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Prepares dependencies for every package manager detected in
/// `workspace_path`. Never fails the caller for an individual ecosystem's
/// install failure (logged as a warning instead) -- a workspace is still
/// usable, just possibly needing the agent to finish installing itself,
/// which is a slower path, not a broken one.
pub fn prepare(workspace_path: &Path) -> Result<()> {
    for manager in detect::detect(workspace_path) {
        let result = match manager {
            PackageManager::Npm => prepare_npm(workspace_path),
            other => passthrough::run(other, workspace_path),
        };
        if let Err(err) = result {
            tracing::warn!("dependency prepare step for {manager:?} failed (continuing): {err:#}");
        }
    }
    Ok(())
}

fn prepare_npm(workspace_path: &Path) -> Result<()> {
    let lockfile = workspace_path.join("package-lock.json");
    if !lockfile.exists() {
        tracing::warn!(
            "no package-lock.json in {}; installing without sharing since there's nothing \
             stable to key a shared cache on",
            workspace_path.display()
        );
        return passthrough::run(PackageManager::Npm, workspace_path).or_else(|_| {
            // PackageManager::Npm intentionally bails in passthrough::run;
            // fall back to a plain, unshared install directly here instead.
            run_plain_npm_install(workspace_path)
        });
    }

    let key = format!("{}-{}", platform_key(), hash_file(&lockfile)?);
    let store = ContentStore::new(store_root_for(workspace_path)?)?;

    let entry = store.populate_if_absent(&key, |tmp| {
        std::fs::copy(
            workspace_path.join("package.json"),
            tmp.join("package.json"),
        )
        .context("copying package.json into store staging dir")?;
        std::fs::copy(&lockfile, tmp.join("package-lock.json"))
            .context("copying package-lock.json into store staging dir")?;

        let output = cmdutil::run("npm", &["ci"], tmp)?;
        if !output.status.success() {
            anyhow::bail!(
                "npm ci failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    })?;

    let node_modules_src = entry.join("node_modules");
    if node_modules_src.exists() {
        ContentStore::materialize(&node_modules_src, &workspace_path.join("node_modules"))?;
    }
    Ok(())
}

fn run_plain_npm_install(workspace_path: &Path) -> Result<()> {
    let output = cmdutil::run("npm", &["install"], workspace_path)?;
    if !output.status.success() {
        tracing::warn!(
            "`npm install` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
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

/// Distinguishes store entries by OS, architecture, and Node major version,
/// since npm packages with native bindings produce non-portable build
/// artifacts across any of those.
fn platform_key() -> String {
    let node_major = cmdutil::run("node", &["--version"], Path::new("."))
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.trim()
                .trim_start_matches('v')
                .split('.')
                .next()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "{}-{}-node{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        node_major
    )
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}
