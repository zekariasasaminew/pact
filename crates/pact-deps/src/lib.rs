//! The dependency broker (Phase 1). Detects a workspace's package
//! manager(s) and makes sure dependencies are ready before the agent's
//! first real command runs -- see DESIGN.md ("pact-deps") for the caching
//! strategy per ecosystem and the real Windows MAX_PATH failure that
//! shaped `prepare_npm`'s fallback path.

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
        return passthrough::run(PackageManager::Npm, workspace_path)
            .or_else(|_| run_plain_npm_install(workspace_path));
    }

    let key = format!("{}-{}", platform_key(), hash_file(&lockfile)?);
    let store = ContentStore::new(store_root_for(workspace_path)?)?;

    let populated = store.populate_if_absent(&key, |tmp| {
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
    });

    let entry = match populated {
        Ok(entry) => entry,
        Err(err) => {
            tracing::warn!(
                "populating the shared npm store failed for key '{key}', falling back to a \
                 normal (unshared) install for this workspace: {err:#}"
            );
            return run_plain_npm_install(workspace_path);
        }
    };

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
