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
}
