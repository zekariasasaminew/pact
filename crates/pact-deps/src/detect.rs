use std::path::Path;

/// A detected package manager for one ecosystem within a project. A single
/// project (e.g. a monorepo) can have several -- one per ecosystem -- so
/// `detect` returns a `Vec`, not a single value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Bun,
    Pnpm,
    Yarn,
    Npm,
    Uv,
    Poetry,
    Pipenv,
    PipPlain,
    Cargo,
    GoModules,
    Maven,
    Gradle,
}

/// Detects package managers in use at `project_root` by marker file. Within
/// one ecosystem family (JS, Python) only one manager is reported, in order
/// of how specifically it can be identified -- e.g. a `pnpm-lock.yaml`
/// takes priority over a bare `package.json`, since the lockfile is
/// unambiguous evidence while `package.json` alone is not.
pub fn detect(project_root: &Path) -> Vec<PackageManager> {
    let mut found = Vec::new();
    let exists = |name: &str| project_root.join(name).exists();

    if exists("bun.lockb") || exists("bun.lock") {
        found.push(PackageManager::Bun);
    } else if exists("pnpm-lock.yaml") {
        found.push(PackageManager::Pnpm);
    } else if exists("yarn.lock") {
        found.push(PackageManager::Yarn);
    } else if exists("package-lock.json") || exists("package.json") {
        found.push(PackageManager::Npm);
    }

    if exists("uv.lock") {
        found.push(PackageManager::Uv);
    } else if exists("poetry.lock") {
        found.push(PackageManager::Poetry);
    } else if exists("Pipfile.lock") || exists("Pipfile") {
        found.push(PackageManager::Pipenv);
    } else if exists("requirements.txt") || exists("setup.py") || exists("pyproject.toml") {
        found.push(PackageManager::PipPlain);
    }

    if exists("Cargo.toml") {
        found.push(PackageManager::Cargo);
    }
    if exists("go.mod") {
        found.push(PackageManager::GoModules);
    }
    if exists("pom.xml") {
        found.push(PackageManager::Maven);
    }
    if exists("build.gradle") || exists("build.gradle.kts") {
        found.push(PackageManager::Gradle);
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pact-deps-detect-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_bun_via_text_lockfile() {
        let dir = scratch_dir("bun-lock");
        fs::write(dir.join("package.json"), "{}").unwrap();
        fs::write(dir.join("bun.lock"), "").unwrap();

        assert_eq!(detect(&dir), vec![PackageManager::Bun]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detects_bun_via_binary_lockfile() {
        let dir = scratch_dir("bun-lockb");
        fs::write(dir.join("package.json"), "{}").unwrap();
        fs::write(dir.join("bun.lockb"), []).unwrap();

        assert_eq!(detect(&dir), vec![PackageManager::Bun]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bun_lockfile_takes_priority_over_bare_package_json_not_reported_as_npm() {
        let dir = scratch_dir("bun-vs-npm");
        fs::write(dir.join("package.json"), "{}").unwrap();
        fs::write(dir.join("package-lock.json"), "{}").unwrap();
        fs::write(dir.join("bun.lock"), "").unwrap();

        let found = detect(&dir);
        assert_eq!(found, vec![PackageManager::Bun], "a bun lockfile must win over an npm one, not report both");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bare_package_json_without_any_lockfile_is_still_npm() {
        let dir = scratch_dir("bare-package-json");
        fs::write(dir.join("package.json"), "{}").unwrap();

        assert_eq!(detect(&dir), vec![PackageManager::Npm]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
