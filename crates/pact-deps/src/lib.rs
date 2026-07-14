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
//!
//! **A real failure mode found while verifying issue #7's fallback path,
//! not a synthetic test case:** the store's key (platform/arch/libc/node/
//! npm version plus a 64-character lockfile hash) makes store-entry paths
//! meaningfully longer than a plain per-workspace `node_modules` would be.
//! Confirmed directly on Windows: `npm ci` populating a store entry for a
//! package with a postinstall step (`esbuild`) failed with `ENOENT`
//! spawning `cmd.exe` -- not because `cmd.exe` was missing, but because
//! the fully-qualified path to the file being installed exceeded Windows'
//! legacy `MAX_PATH` (260 chars) once nested under a long store-key
//! directory name inside an already-long temp/state-dir root. This is
//! exactly the class of precondition-not-met failure `prepare_npm`'s
//! populate-failure fallback (see below) exists for: it was hit for real,
//! not hypothetically, and the fallback to a plain per-workspace install
//! (a shorter path) succeeded where the store population didn't.

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

    // A store-population failure (network blip, a native build tool
    // missing on this specific machine, a registry issue) shouldn't leave
    // the workspace with no node_modules at all -- fall back to a normal,
    // unshared install for this one workspace instead, the same as the
    // no-lockfile path already does. See issue #7's risk analysis: this
    // was a real gap, not just a hypothetical one.
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
/// only), Node major version, and npm's own version -- see issue #7's risk
/// analysis for why each of these, beyond the original os/arch/node-major
/// set, turned out to matter: npm version because different npm versions
/// can lay out `node_modules` differently from an identical lockfile, and
/// libc flavor because packages that resolve a platform-specific binary
/// via `optionalDependencies` (esbuild, swc, sharp, and others in that
/// exact shape) pick a *different* one for musl (Alpine) vs. glibc
/// (Debian/Ubuntu) despite both reporting the same os/arch.
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
/// detected via the presence of a musl dynamic linker, which is how musl
/// libc (Alpine's default) identifies itself; anything else on Linux is
/// assumed glibc. Best-effort: if detection is inconclusive, "glibc" is
/// the safer assumption (it's the overwhelming majority of non-Alpine
/// Linux), not silently omitting the dimension entirely.
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
