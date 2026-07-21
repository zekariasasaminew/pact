//! Integration coverage for `WorkspaceManager::merge_all` against a real,
//! throwaway git repo -- see DESIGN.md ("pact-vcs > merge_all").
use std::path::{Path, PathBuf};
use std::process::Command;

use pact_vcs::WorkspaceManager;
use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "`git {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Creates a fresh temp git repo with two committed files: `src/index.ts`
/// (five lines, so an append at the end has real surrounding context and a
/// change to line 1 stays a well-separated edit) and `src/other.ts` (one
/// line, an entirely unrelated file). Returns the repo root.
fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-vcs-merge-all-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(
        root.join("src/index.ts"),
        "export const L1 = 1;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();
    std::fs::write(root.join("src/other.ts"), "export const OTHER = 0;\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// Same base as `init_repo`, plus `package.json` (one existing dependency)
/// and a stand-in `package-lock.json` -- used by the semantic-resolution
/// tests below.
fn init_repo_with_package_json() -> PathBuf {
    let root = init_repo();
    std::fs::write(
        root.join("package.json"),
        "{\n  \"name\": \"test\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\n    \"a\": \"1.0.0\"\n  }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("package-lock.json"),
        "{\n  \"lockfileVersion\": 1\n}\n",
    )
    .unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "add package.json"]);
    root
}

/// Same base as `init_repo`, plus a single-line `src/barrel.ts` -- used by
/// the `--union` test. See DESIGN.md ("pact-vcs > merge_all") for why a
/// single-line file is used deliberately.
fn init_repo_with_barrel() -> PathBuf {
    let root = init_repo();
    std::fs::write(root.join("src/barrel.ts"), "export {};\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "add barrel"]);
    root
}

fn show(repo: &Path, spec: &str) -> String {
    let output = Command::new("git")
        .args(["show", spec])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "git show {spec} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn merge_all_merges_compatible_changes_and_skips_real_conflict() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    // See DESIGN.md ("pact-vcs > merge_all") for why this scenario is
    // shaped the way it is (confirmed by hand against real git first).
    let a = manager.create_workspace("append L6").unwrap();
    std::fs::write(
        a.path.join("src/index.ts"),
        "export const L1 = 1;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\nexport const L6 = 6;\n",
    )
    .unwrap();

    let b = manager.create_workspace("bump OTHER").unwrap();
    std::fs::write(b.path.join("src/other.ts"), "export const OTHER = 99;\n").unwrap();

    let c = manager.create_workspace("bump L1 to 100").unwrap();
    std::fs::write(
        c.path.join("src/index.ts"),
        "export const L1 = 100;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let d = manager.create_workspace("bump L1 to 200").unwrap();
    std::fs::write(
        d.path.join("src/index.ts"),
        "export const L1 = 200;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let report = manager.merge_all(None, None, &[], None, false).unwrap();

    let merged_ids: Vec<&str> = report.merged.iter().map(|w| w.id.as_str()).collect();
    assert!(merged_ids.contains(&a.id.as_str()), "expected {} (well-separated append) to always merge cleanly", a.id);
    assert!(merged_ids.contains(&b.id.as_str()), "expected {} (unrelated file) to always merge cleanly", b.id);

    // C and D touch the same single file so they tie on the
    // smallest-changeset-first heuristic -- which merges first (and
    // therefore which one the *other* conflicts against) isn't specified.
    let c_merged = merged_ids.contains(&c.id.as_str());
    let d_merged = merged_ids.contains(&d.id.as_str());
    assert!(
        c_merged ^ d_merged,
        "expected exactly one of C/D (they conflict with each other) to merge, got merged={merged_ids:?}"
    );
    assert_eq!(report.merged.len(), 3, "expected A, B, and exactly one of C/D to merge, got {merged_ids:?}");

    assert_eq!(report.skipped.len(), 1, "expected exactly one skipped workspace");
    let skipped = &report.skipped[0];
    assert!(
        skipped.id == c.id || skipped.id == d.id,
        "expected the skipped workspace to be C or D, got {}", skipped.id
    );
    assert!(
        skipped.reason.contains("merge conflict"),
        "expected a merge-conflict reason, got: {}",
        skipped.reason
    );

    // The report's branch must actually exist in the repo, containing A and
    // B's changes, and the repo's own checkout must be untouched (still on
    // whatever branch `git init` created, not the integration branch).
    let branches = String::from_utf8(
        Command::new("git")
            .args(["branch", "--list", &report.target_branch])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        branches.contains(&report.target_branch),
        "expected branch '{}' to exist after merge_all", report.target_branch
    );

    let current_branch = String::from_utf8(
        Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        !current_branch.trim().is_empty() && current_branch.trim() != report.target_branch,
        "merge_all must not check out the integration branch in the repo's own working tree"
    );

    cleanup(&repo);
}

#[test]
fn merge_all_dry_run_touches_no_git_state() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add chunk export").unwrap();
    std::fs::write(
        a.path.join("src/index.ts"),
        "export {};\nexport * from './chunk';\n",
    )
    .unwrap();

    let branches_before = String::from_utf8(
        Command::new("git").args(["branch"]).current_dir(&repo).output().unwrap().stdout,
    )
    .unwrap();

    let report = manager.merge_all(None, None, &[], None, true).unwrap();

    assert!(report.dry_run);
    assert!(report.merged.is_empty(), "dry run must not actually merge anything");
    assert_eq!(report.planned, vec![a.id.clone()]);

    let branches_after = String::from_utf8(
        Command::new("git").args(["branch"]).current_dir(&repo).output().unwrap().stdout,
    )
    .unwrap();
    assert_eq!(
        branches_before, branches_after,
        "dry run must not create the integration branch (or any other branch)"
    );

    cleanup(&repo);
}

#[test]
fn merge_all_skips_workspace_whose_base_is_no_longer_an_ancestor() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("some change").unwrap();
    std::fs::write(a.path.join("src/index.ts"), "export const X = 1;\n").unwrap();

    // Rewrite the repo's own init commit so the SHA `a.base_commit` recorded
    // no longer exists on this branch's history at all -- simulating "HEAD
    // was reset/rebased since this workspace was created" without needing a
    // second, unrelated repo.
    run_git(&repo, &["commit", "--amend", "-q", "--allow-empty", "-m", "init (amended)"]);

    let report = manager.merge_all(None, None, &[], None, false).unwrap();

    assert!(report.merged.is_empty(), "workspace with a moved base must not be merged");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].id, a.id);
    assert!(
        report.skipped[0].reason.contains("no longer part of this branch's history"),
        "expected a moving-base reason, got: {}",
        report.skipped[0].reason
    );

    cleanup(&repo);
}

#[test]
fn merge_all_auto_resolves_package_json_dependency_conflict() {
    let repo = init_repo_with_package_json();
    let manager = WorkspaceManager::open(&repo).unwrap();

    // Confirmed by hand against real git first -- see DESIGN.md
    // ("pact-vcs > Semantic auto-resolution").
    let a = manager.create_workspace("add dep b").unwrap();
    std::fs::write(
        a.path.join("package.json"),
        "{\n  \"name\": \"test\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\n    \"a\": \"1.0.0\",\n    \"b\": \"2.0.0\"\n  }\n}\n",
    )
    .unwrap();

    let b = manager.create_workspace("add dep c").unwrap();
    std::fs::write(
        b.path.join("package.json"),
        "{\n  \"name\": \"test\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\n    \"a\": \"1.0.0\",\n    \"c\": \"3.0.0\"\n  }\n}\n",
    )
    .unwrap();

    let report = manager.merge_all(None, None, &[], None, false).unwrap();

    assert_eq!(report.skipped.len(), 0, "expected both to merge via JSON-aware auto-resolution, got skipped={:?}", report.skipped);
    assert_eq!(report.merged.len(), 2);

    // One of the two merges is a plain clean merge (whichever lands first
    // on an untouched package.json); the other must go through -- and
    // report -- the JSON-aware resolver.
    let auto_resolved_count = report
        .merged
        .iter()
        .filter(|w| w.auto_resolved.iter().any(|f| f == "package.json"))
        .count();
    assert_eq!(auto_resolved_count, 1, "expected exactly one merge to need package.json auto-resolution");

    let content = show(&repo, &format!("{}:package.json", report.target_branch));
    let value: serde_json::Value = serde_json::from_str(&content).expect("merged package.json must be valid JSON");
    let deps = value.get("dependencies").and_then(|d| d.as_object()).expect("dependencies object");
    assert_eq!(deps.get("a").and_then(|v| v.as_str()), Some("1.0.0"));
    assert_eq!(deps.get("b").and_then(|v| v.as_str()), Some("2.0.0"));
    assert_eq!(deps.get("c").and_then(|v| v.as_str()), Some("3.0.0"));

    cleanup(&repo);
}

/// Regression test for issue #29: the JSON-aware package.json resolver used
/// to reserialize through a BTreeMap-backed `serde_json::Value`, which
/// alphabetized every top-level key (`dependencies` before `name`) and
/// hardcoded 2-space indent regardless of the file's own convention. Uses a
/// 4-space-indented file with the full standard npm key set, deliberately
/// not already alphabetical, so either regression would show up.
#[test]
fn merge_all_package_json_merge_preserves_key_order_and_indent() {
    let repo = init_repo();
    let base = "{\n    \"name\": \"widget\",\n    \"version\": \"1.0.0\",\n    \
                \"description\": \"a widget\",\n    \"main\": \"index.js\",\n    \
                \"scripts\": {\n        \"test\": \"node test.js\"\n    },\n    \
                \"dependencies\": {\n        \"a\": \"1.0.0\"\n    },\n    \
                \"devDependencies\": {}\n}\n";
    std::fs::write(repo.join("package.json"), base).unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add 4-space package.json"]);

    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add dep b").unwrap();
    std::fs::write(
        a.path.join("package.json"),
        base.replace("\"a\": \"1.0.0\"\n", "\"a\": \"1.0.0\",\n        \"b\": \"2.0.0\"\n"),
    )
    .unwrap();

    let b = manager.create_workspace("add dep c").unwrap();
    std::fs::write(
        b.path.join("package.json"),
        base.replace("\"a\": \"1.0.0\"\n", "\"a\": \"1.0.0\",\n        \"c\": \"3.0.0\"\n"),
    )
    .unwrap();

    let report = manager.merge_all(None, None, &[], None, false).unwrap();
    assert_eq!(report.skipped.len(), 0, "expected both to merge via JSON-aware auto-resolution, got skipped={:?}", report.skipped);

    let content = show(&repo, &format!("{}:package.json", report.target_branch));

    let value: serde_json::Value = serde_json::from_str(&content).expect("merged package.json must be valid JSON");
    let keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        vec!["name", "version", "description", "main", "scripts", "dependencies", "devDependencies"],
        "top-level key order must be preserved across a merge, got {keys:?}\nfull content:\n{content}"
    );

    assert!(
        content.contains("\n    \"name\": \"widget\""),
        "expected the file's own 4-space indent to be preserved, got:\n{content}"
    );
    assert!(content.contains("\"a\": \"1.0.0\""));
    assert!(content.contains("\"b\": \"2.0.0\""));
    assert!(content.contains("\"c\": \"3.0.0\""));

    cleanup(&repo);
}

/// Regression test for issue #57: a real Windows footgun where PowerShell's
/// `Out-File -Encoding utf8` (and other common tooling) writes a leading
/// UTF-8 BOM by default. `serde_json::from_str` rejects a BOM outright, so
/// before the fix this fell through to a real conflict instead of the
/// JSON-aware auto-resolve.
#[test]
fn merge_all_package_json_merge_handles_utf8_bom() {
    let repo = init_repo();
    const BOM: &str = "\u{FEFF}";
    let base = "{\n  \"name\": \"widget\",\n  \"version\": \"1.0.0\",\n  \
                \"dependencies\": {\n    \"a\": \"1.0.0\"\n  }\n}\n";
    std::fs::write(repo.join("package.json"), format!("{BOM}{base}")).unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add BOM'd package.json"]);

    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add dep b").unwrap();
    std::fs::write(
        a.path.join("package.json"),
        format!("{BOM}{}", base.replace("\"a\": \"1.0.0\"\n", "\"a\": \"1.0.0\",\n    \"b\": \"2.0.0\"\n")),
    )
    .unwrap();

    let b = manager.create_workspace("add dep c").unwrap();
    std::fs::write(
        b.path.join("package.json"),
        format!("{BOM}{}", base.replace("\"a\": \"1.0.0\"\n", "\"a\": \"1.0.0\",\n    \"c\": \"3.0.0\"\n")),
    )
    .unwrap();

    let report = manager.merge_all(None, None, &[], None, false).unwrap();
    assert_eq!(
        report.skipped.len(),
        0,
        "a BOM'd package.json must still auto-resolve via the JSON-aware merge, got skipped={:?}",
        report.skipped
    );

    let content = show(&repo, &format!("{}:package.json", report.target_branch));
    let value: serde_json::Value = serde_json::from_str(&content).expect("merged package.json must be valid JSON");
    let deps = value.get("dependencies").and_then(|d| d.as_object()).expect("dependencies object");
    assert_eq!(deps.get("a").and_then(|v| v.as_str()), Some("1.0.0"));
    assert_eq!(deps.get("b").and_then(|v| v.as_str()), Some("2.0.0"));
    assert_eq!(deps.get("c").and_then(|v| v.as_str()), Some("3.0.0"));

    cleanup(&repo);
}

#[test]
fn merge_all_union_resolves_matched_file_conflict() {
    let repo = init_repo_with_barrel();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("export chunk").unwrap();
    std::fs::write(a.path.join("src/barrel.ts"), "export {};\nexport * from './chunk';\n").unwrap();

    let b = manager.create_workspace("export omit").unwrap();
    std::fs::write(b.path.join("src/barrel.ts"), "export {};\nexport * from './omit';\n").unwrap();

    let report = manager
        .merge_all(None, None, &["src/barrel.ts".to_string()], None, false)
        .unwrap();

    assert_eq!(report.skipped.len(), 0, "expected both to merge via --union, got skipped={:?}", report.skipped);
    assert_eq!(report.merged.len(), 2);

    let auto_resolved_count = report
        .merged
        .iter()
        .filter(|w| w.auto_resolved.iter().any(|f| f == "src/barrel.ts"))
        .count();
    assert_eq!(auto_resolved_count, 1, "expected exactly one merge to need the union resolver");

    let content = show(&repo, &format!("{}:src/barrel.ts", report.target_branch));
    assert!(content.contains("export * from './chunk';"));
    assert!(content.contains("export * from './omit';"));

    cleanup(&repo);
}

#[test]
fn merge_all_never_auto_resolves_lockfiles_even_with_matching_union_glob() {
    let repo = init_repo_with_package_json();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("touch lockfile a").unwrap();
    std::fs::write(
        a.path.join("package-lock.json"),
        "{\n  \"lockfileVersion\": 1,\n  \"a\": true\n}\n",
    )
    .unwrap();

    let b = manager.create_workspace("touch lockfile b").unwrap();
    std::fs::write(
        b.path.join("package-lock.json"),
        "{\n  \"lockfileVersion\": 1,\n  \"b\": true\n}\n",
    )
    .unwrap();

    // Even with an explicit --union match on the lockfile, NEVER_AUTO_RESOLVE
    // must win -- a real conflict here always stays a real conflict.
    let report = manager
        .merge_all(None, None, &["package-lock.json".to_string()], None, false)
        .unwrap();

    assert_eq!(report.merged.len(), 1, "expected exactly one of the two to merge cleanly");
    assert_eq!(report.skipped.len(), 1, "expected the other to stay a real, unresolved conflict");
    assert!(report.skipped[0].reason.contains("merge conflict"));

    cleanup(&repo);
}

/// Regression test for a real Windows shakedown finding: two agents each
/// append a disjoint export to the same CommonJS barrel and its
/// `module.exports` line. Both sides touch the *same* last line
/// differently, so git's plain 3-way merge always conflicts here (confirmed
/// by hand, same lesson as `init_repo_with_barrel`'s doc comment) -- and
/// the union resolver must recognize the merged result would carry two
/// `module.exports =` assignments (second silently wins) and refuse it,
/// rather than reporting a broken merge as `auto-resolved`.
#[test]
fn merge_all_union_rejects_conflicting_module_exports() {
    let repo = init_repo();
    std::fs::write(
        repo.join("src/barrel.js"),
        "const { add } = require('./add');\nconst { sub } = require('./sub');\nmodule.exports = { add, sub };\n",
    )
    .unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add js barrel"]);

    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add mul").unwrap();
    std::fs::write(
        a.path.join("src/barrel.js"),
        "const { add } = require('./add');\nconst { sub } = require('./sub');\n\
         const { mul } = require('./mul');\nmodule.exports = { add, sub, mul };\n",
    )
    .unwrap();

    let b = manager.create_workspace("add div").unwrap();
    std::fs::write(
        b.path.join("src/barrel.js"),
        "const { add } = require('./add');\nconst { sub } = require('./sub');\n\
         const { div } = require('./div');\nmodule.exports = { add, sub, div };\n",
    )
    .unwrap();

    let report = manager
        .merge_all(None, None, &["src/barrel.js".to_string()], None, false)
        .unwrap();

    assert_eq!(report.merged.len(), 1, "expected exactly one of the two to merge cleanly");
    assert_eq!(
        report.skipped.len(),
        1,
        "a union merge that would carry two `module.exports =` statements must stay a real conflict, not be silently auto-resolved -- got merged={:?} skipped={:?}",
        report.merged,
        report.skipped
    );
    assert!(report.skipped[0].reason.contains("merge conflict"));

    cleanup(&repo);
}

#[test]
fn merge_all_accepts_a_stub_arbiter_resolution() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let c = manager.create_workspace("bump L1 to 100").unwrap();
    std::fs::write(
        c.path.join("src/index.ts"),
        "export const L1 = 100;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();
    let d = manager.create_workspace("bump L1 to 200").unwrap();
    std::fs::write(
        d.path.join("src/index.ts"),
        "export const L1 = 200;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    // Stands in for pact-core's real Arbiter closure (which would spawn an
    // agent and run a test command) -- proves the wiring end to end
    // (unresolved files reach the closure, its resolution gets staged and
    // committed, `arbiter_resolved` is reported) without spawning a real,
    // billed agent.
    let resolver = |worktree_path: &Path, _task_text: &str, files: &[String]| -> Vec<String> {
        for file in files {
            std::fs::write(
                worktree_path.join(file),
                "export const L1 = 999;\nexport const L2 = 2;\nexport const L3 = 3;\n\
                 export const L4 = 4;\nexport const L5 = 5;\n",
            )
            .unwrap();
            let add = Command::new("git").args(["add", "--", file]).current_dir(worktree_path).output();
            assert!(matches!(add, Ok(o) if o.status.success()));
        }
        files.to_vec()
    };

    let report = manager.merge_all(None, None, &[], Some(&resolver), false).unwrap();

    assert_eq!(report.skipped.len(), 0, "expected the stub arbiter to resolve the conflict, got skipped={:?}", report.skipped);
    assert_eq!(report.merged.len(), 2);
    let arbiter_resolved_count = report
        .merged
        .iter()
        .filter(|w| w.arbiter_resolved.iter().any(|f| f == "src/index.ts"))
        .count();
    assert_eq!(arbiter_resolved_count, 1, "expected exactly one merge to have needed the arbiter");

    let content = show(&repo, &format!("{}:src/index.ts", report.target_branch));
    assert!(content.contains("export const L1 = 999;"));

    cleanup(&repo);
}

#[test]
fn merge_all_still_aborts_when_arbiter_declines() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let c = manager.create_workspace("bump L1 to 100").unwrap();
    std::fs::write(
        c.path.join("src/index.ts"),
        "export const L1 = 100;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();
    let d = manager.create_workspace("bump L1 to 200").unwrap();
    std::fs::write(
        d.path.join("src/index.ts"),
        "export const L1 = 200;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    // A "verification failed" stub -- e.g. its test command didn't pass --
    // always declines regardless of input. The merge must still be aborted
    // and reported as a real conflict, exactly as if Arbiter weren't
    // configured at all.
    let resolver = |_worktree_path: &Path, _task_text: &str, _files: &[String]| -> Vec<String> { Vec::new() };

    let report = manager.merge_all(None, None, &[], Some(&resolver), false).unwrap();

    assert_eq!(report.merged.len(), 1, "expected exactly one of the two to merge cleanly");
    assert_eq!(report.skipped.len(), 1, "expected the other to stay a real conflict when arbiter declines");

    cleanup(&repo);
}
