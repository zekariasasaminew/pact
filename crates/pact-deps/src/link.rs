//! Link mode: share the repo root's already-installed `node_modules` with
//! a workspace through a directory link instead of running a full install
//! there -- see DESIGN.md ("pact-deps > Link mode", issue #283). A junction
//! on Windows (no privilege needed, unlike a directory symlink), a plain
//! symlink elsewhere.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// The one directory link mode shares for JavaScript ecosystems.
pub const NODE_MODULES: &str = "node_modules";

/// Whether `repo_root` has an install that a workspace could link to.
pub fn shareable_node_modules(repo_root: &Path) -> Option<std::path::PathBuf> {
    let candidate = repo_root.join(NODE_MODULES);
    // A link at the repo root itself (a user's own junction into a shared
    // store, say) is fine to link through, so `exists` rather than
    // `is_dir` on the symlink metadata.
    candidate.is_dir().then_some(candidate)
}

/// Creates `link` pointing at `target`. Refuses to replace a real
/// directory at `link` (that would be someone's actual install); replaces
/// an existing link so a re-prepare converges instead of failing.
pub fn link_dir(target: &Path, link: &Path) -> Result<()> {
    if let Ok(md) = std::fs::symlink_metadata(link) {
        if md.file_type().is_symlink() {
            remove_link(link).with_context(|| format!("replacing existing link {}", link.display()))?;
        } else {
            bail!(
                "{} already exists and is not a link; refusing to replace a real directory",
                link.display()
            );
        }
    }
    create_link(target, link).with_context(|| format!("linking {} -> {}", link.display(), target.display()))
}

/// Makes sure git ignores the link named `name` at the root of
/// `workspace`. A `.gitignore` pattern with a trailing slash
/// (`node_modules/`, GitHub's own Node template) matches directories only,
/// and on Unix a symlink is not one, so a freshly linked `node_modules`
/// would otherwise show up as untracked: `pact list` would call the
/// workspace dirty, `teardown` would refuse without `--force`, and
/// `commit_all` would commit the link itself. (Git for Windows treats a
/// junction as a directory, so the pattern matches there.) When `git
/// check-ignore` says the link is not ignored, appends an anchored,
/// slash-free pattern to the repository's `info/exclude` -- shared by
/// every worktree of the repo and never committed -- and returns a note
/// saying so. `None` when nothing needed doing, including when
/// `workspace` is not a git checkout at all.
pub fn ensure_git_ignores(workspace: &Path, name: &str) -> Option<String> {
    if git_ignores(workspace, name)? {
        return None;
    }
    let exclude = git_text(workspace, &["rev-parse", "--git-path", "info/exclude"])?;
    let exclude_path = workspace.join(exclude.trim());
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&exclude_path).ok()?;
    use std::io::Write as _;
    writeln!(
        file,
        "\n# added by pact: `{name}` is a link here, which a trailing-slash .gitignore pattern does not match\n/{name}"
    )
    .ok()?;
    if git_ignores(workspace, name)? {
        Some(format!(
            "'{name}' was not ignored by this repo's .gitignore once it became a link (trailing-slash \
             patterns match directories only); added '/{name}' to {}",
            exclude_path.display()
        ))
    } else {
        Some(format!(
            "'{name}' is still not ignored by git after adding '/{name}' to {}; the workspace will show \
             the link as an untracked change",
            exclude_path.display()
        ))
    }
}

/// `Some(true)` if git ignores `name` in `workspace`, `Some(false)` if it
/// doesn't, `None` if `workspace` isn't a git checkout (or git isn't
/// available).
fn git_ignores(workspace: &Path, name: &str) -> Option<bool> {
    let status = std::process::Command::new("git")
        .args(["check-ignore", "-q", "--", name])
        .current_dir(workspace)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

fn git_text(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).current_dir(workspace).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(windows)]
fn create_link(target: &Path, link: &Path) -> std::io::Result<()> {
    junction::create(target, link)
}

#[cfg(not(windows))]
fn create_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn remove_link(link: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(link).or_else(|_| std::fs::remove_file(link))
}

#[cfg(not(windows))]
fn remove_link(link: &Path) -> std::io::Result<()> {
    std::fs::remove_file(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pact-deps-link-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn link_dir_makes_the_target_reachable_through_the_link() {
        let base = scratch("reachable");
        let target = base.join("target");
        std::fs::create_dir_all(target.join("pkg")).unwrap();
        std::fs::write(target.join("pkg").join("index.js"), "module.exports = 1;").unwrap();
        let link = base.join("ws").join(NODE_MODULES);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();

        link_dir(&target, &link).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(link.join("pkg").join("index.js")).unwrap(), "module.exports = 1;");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn link_dir_replaces_an_existing_link_but_refuses_a_real_directory() {
        let base = scratch("replace");
        let first = base.join("first");
        let second = base.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("marker"), "2").unwrap();
        let link = base.join(NODE_MODULES);

        link_dir(&first, &link).unwrap();
        link_dir(&second, &link).unwrap();
        assert!(link.join("marker").exists(), "re-linking must point at the new target");

        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let err = link_dir(&first, &real).unwrap_err();
        assert!(err.to_string().contains("not a link"), "got: {err:#}");
        assert!(real.is_dir(), "the real directory must be left alone");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shareable_node_modules_requires_a_directory_at_the_repo_root() {
        let base = scratch("shareable");
        assert_eq!(shareable_node_modules(&base), None);
        std::fs::create_dir_all(base.join(NODE_MODULES)).unwrap();
        assert_eq!(shareable_node_modules(&base), Some(base.join(NODE_MODULES)));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(output.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn git_repo_with_ignore(name: &str, ignore_line: &str) -> std::path::PathBuf {
        let repo = scratch(name);
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), format!("{ignore_line}\n")).unwrap();
        repo
    }

    #[test]
    fn ensure_git_ignores_leaves_an_already_ignored_link_alone() {
        // A slash-free pattern matches a symlink on every platform, so
        // nothing needs adding and info/exclude stays untouched.
        let repo = git_repo_with_ignore("ignored", NODE_MODULES);
        let target = repo.join("target");
        std::fs::create_dir_all(&target).unwrap();
        link_dir(&target, &repo.join(NODE_MODULES)).unwrap();

        assert_eq!(ensure_git_ignores(&repo, NODE_MODULES), None);
        assert!(!repo.join(".git").join("info").join("exclude").exists() || {
            let exclude = std::fs::read_to_string(repo.join(".git").join("info").join("exclude")).unwrap();
            !exclude.contains("added by pact")
        });
        assert_eq!(git(&repo, &["status", "--porcelain"]).trim(), "?? .gitignore");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn ensure_git_ignores_excludes_a_link_a_trailing_slash_pattern_misses() {
        let repo = git_repo_with_ignore("trailing-slash", "node_modules/");
        let target = repo.join("target");
        std::fs::create_dir_all(&target).unwrap();
        link_dir(&target, &repo.join(NODE_MODULES)).unwrap();

        let note = ensure_git_ignores(&repo, NODE_MODULES);
        // Git for Windows treats a junction as a directory, so the
        // trailing-slash pattern already matches there and no note is
        // produced; on Unix the symlink is not a directory and the
        // exclude must be added. Either way the link must end up ignored.
        if cfg!(windows) {
            assert_eq!(note, None);
        } else {
            assert!(note.as_deref().is_some_and(|n| n.contains("added '/node_modules'")), "got: {note:?}");
            let exclude = std::fs::read_to_string(repo.join(".git").join("info").join("exclude")).unwrap();
            assert!(exclude.contains("/node_modules"), "info/exclude: {exclude}");
        }
        let status = git(&repo, &["status", "--porcelain"]);
        assert!(!status.contains("node_modules"), "the link must not appear as untracked; status: {status}");
        assert_eq!(ensure_git_ignores(&repo, NODE_MODULES), None, "a second call must be a no-op");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn ensure_git_ignores_is_a_no_op_outside_a_git_checkout() {
        let dir = scratch("not-a-repo");
        assert_eq!(ensure_git_ignores(&dir, NODE_MODULES), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_git_ignores_adds_an_exclude_when_nothing_ignores_the_link() {
        // Exercises the exclude-writing branch on every platform (the
        // trailing-slash case above only reaches it on Unix): a repo with
        // no ignore rule for node_modules at all.
        let repo = git_repo_with_ignore("no-rule", "dist/");
        let target = repo.join("target");
        std::fs::create_dir_all(target.join("pkg")).unwrap();
        std::fs::write(target.join("pkg").join("index.js"), "x").unwrap();
        link_dir(&target, &repo.join(NODE_MODULES)).unwrap();
        assert!(git(&repo, &["status", "--porcelain"]).contains("node_modules"), "precondition: the link shows as untracked");

        let note = ensure_git_ignores(&repo, NODE_MODULES).expect("an exclude must be added");
        assert!(note.contains("added '/node_modules'"), "got: {note}");
        let exclude = std::fs::read_to_string(repo.join(".git").join("info").join("exclude")).unwrap();
        assert!(exclude.contains("added by pact") && exclude.contains("/node_modules"), "info/exclude: {exclude}");
        let status = git(&repo, &["status", "--porcelain"]);
        assert!(!status.contains("node_modules"), "status: {status}");
        assert_eq!(ensure_git_ignores(&repo, NODE_MODULES), None, "a second call must be a no-op");
        let _ = std::fs::remove_dir_all(&repo);
    }
}
