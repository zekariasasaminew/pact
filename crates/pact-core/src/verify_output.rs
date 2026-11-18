//! Failing files from a verification command's output (issue #373): the
//! repository paths a test runner, type-checker, linter or compiler names,
//! each with the lines around its mentions. Tool-agnostic on purpose: a
//! token counts when, cleaned of the decorations tools add (`:line:col`,
//! tsc's `(line,col)`, quotes, the tree's own absolute prefix), it names a
//! file that exists in the tree.

use std::path::Path;

/// One file the output names, with what it said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailingFile {
    /// Repo-relative, `/`-separated.
    pub path: String,
    pub excerpt: String,
}

const LINES_AFTER_A_MENTION: usize = 4;
const MAX_EXCERPT_LINES: usize = 20;

/// The tree's files named in `output`, in order of first mention, with up
/// to [`MAX_EXCERPT_LINES`] lines each: every line naming the file and the
/// few lines after it up to a blank line, which is where eslint, vitest
/// and cargo put the error under its file.
pub fn failing_files(output: &str, root: &Path) -> Vec<FailingFile> {
    let root_prefix = normalize(&root.to_string_lossy()).trim_end_matches('/').to_string() + "/";
    let lines: Vec<&str> = output.lines().collect();
    let mut found: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        for token in line.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | '=' | ',')) {
            let Some(path) = repo_path(token, &root_prefix) else { continue };
            if path.split('/').any(|segment| segment == "node_modules" || segment == ".git") || !root.join(&path).is_file() {
                continue;
            }
            match found.iter_mut().find(|(known, _)| *known == path) {
                Some((_, mentions)) => {
                    if mentions.last() != Some(&index) {
                        mentions.push(index);
                    }
                }
                None => found.push((path, vec![index])),
            }
        }
    }
    found
        .into_iter()
        .map(|(path, mentions)| {
            let mut taken: Vec<usize> = Vec::new();
            for start in mentions {
                for (offset, line) in lines.iter().enumerate().skip(start).take(LINES_AFTER_A_MENTION + 1) {
                    if offset > start && line.trim().is_empty() {
                        break;
                    }
                    if !taken.contains(&offset) {
                        taken.push(offset);
                    }
                }
            }
            taken.sort_unstable();
            let mut excerpt: Vec<&str> = taken.iter().take(MAX_EXCERPT_LINES).map(|&i| lines[i]).collect();
            if taken.len() > MAX_EXCERPT_LINES {
                excerpt.push("...");
            }
            FailingFile { path, excerpt: excerpt.join("\n") }
        })
        .collect()
}

fn normalize(text: &str) -> String {
    text.replace('\\', "/").trim_start_matches("//?/").to_string()
}

/// `token` as a repo-relative path, or `None` when it cannot be one.
fn repo_path(token: &str, root_prefix: &str) -> Option<String> {
    let mut text = token.trim_start_matches(['(', '[', '{']);
    // tsc: `src/a.ts(10,5):`. A route group such as `(auth)` is followed by
    // a letter, so only a parenthesis followed by a digit is cut.
    if let Some(at) = text.char_indices().find(|&(i, c)| c == '(' && text[i + 1..].starts_with(|d: char| d.is_ascii_digit())).map(|(i, _)| i) {
        text = &text[..at];
    }
    let mut text = text.trim_end_matches(['.', ',', ';', ')', ']', '}']);
    loop {
        let trimmed = text.trim_end_matches(':');
        match trimmed.rfind(':') {
            Some(at) if at + 1 < trimmed.len() && trimmed[at + 1..].chars().all(|c| c.is_ascii_digit()) => text = &trimmed[..at],
            _ => {
                text = trimmed;
                break;
            }
        }
    }
    let mut path = normalize(text);
    let lower = path.to_ascii_lowercase();
    if lower.starts_with(&root_prefix.to_ascii_lowercase()) {
        path = path[root_prefix.len()..].to_string();
    }
    let path = path.trim_start_matches("./").to_string();
    let absolute = path.starts_with('/') || path.chars().nth(1) == Some(':');
    if path.is_empty() || absolute || path.split('/').any(|segment| segment == "..") || !path.contains('.') {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("pact-failing-files-{}", uuid::Uuid::new_v4()));
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        root
    }

    fn paths(found: &[FailingFile]) -> Vec<&str> {
        found.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn the_shapes_tsc_eslint_vitest_and_cargo_print_all_resolve_to_repo_paths() {
        let root = tree(&["src/a.ts", "src/b.tsx", "app/(auth)/[id]/page.tsx", "lib/c.test.ts", "crates/x/src/lib.rs", "package.json"]);
        let abs = root.join("src").join("b.tsx").display().to_string();
        let output = format!(
            "src/a.ts(10,5): error TS1005: ';' expected.\n\
             \n\
             {abs}\n  3:7  error  Unexpected any  @typescript-eslint/no-explicit-any\n\n\
             \u{20}FAIL  lib/c.test.ts > adds > works\nAssertionError: expected 1 to be 2\n\n\
             app/(auth)/[id]/page.tsx:4:1 - error\n\
             \u{20}  --> crates\\x\\src\\lib.rs:695:5\n\
             at Object.<anonymous> (node_modules/vitest/dist/index.js:1:1)\n\
             src/missing.ts(1,1): error\n"
        );
        let found = failing_files(&output, &root);
        assert_eq!(paths(&found), ["src/a.ts", "src/b.tsx", "lib/c.test.ts", "app/(auth)/[id]/page.tsx", "crates/x/src/lib.rs"]);
        assert!(found[1].excerpt.contains("no-explicit-any"), "the error under eslint's file header comes along: {}", found[1].excerpt);
        assert!(found[2].excerpt.contains("expected 1 to be 2"), "{}", found[2].excerpt);
        assert!(!found[1].excerpt.contains("FAIL"), "a blank line ends the excerpt: {}", found[1].excerpt);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_excerpt_is_bounded_and_files_outside_the_tree_are_ignored() {
        let root = tree(&["a.ts"]);
        let output = (0..40).map(|i| format!("a.ts:{i}:1 error {i}")).collect::<Vec<_>>().join("\n") + "\n/etc/hosts\n../a.ts\n";
        let found = failing_files(&output, &root);
        assert_eq!(paths(&found), ["a.ts"]);
        assert_eq!(found[0].excerpt.lines().count(), MAX_EXCERPT_LINES + 1, "{}", found[0].excerpt);
        assert!(found[0].excerpt.ends_with("..."));
        let _ = std::fs::remove_dir_all(&root);
    }
}
