//! Pluggable semantic conflict resolvers -- see DESIGN.md ("pact-vcs >
//! Semantic auto-resolution"). Extracted from two previously hardcoded
//! resolution paths in `WorkspaceManager::try_auto_resolve` (issue #151):
//! `PackageJsonResolver` (JSON-aware dependency-block merge) and
//! `UnionResolver` (plain line-union merge for `--append-only`-matched
//! files). Both existed before this module; this only gives them a common
//! interface so a third resolver (Cargo.toml, pyproject.toml, go.mod, a
//! changelog's append-only section, ...) has somewhere to plug in without
//! `try_auto_resolve` growing another `if`/`else if` arm.

use std::path::Path;

use anyhow::{Context, Result};

/// Git's three conflict stages for one file -- 1 (common ancestor), 2
/// ("ours"), 3 ("theirs") -- plus the path they belong to. Each stage's
/// `bool` is whether that stage's content had a leading UTF-8 BOM (see
/// `WorkspaceManager::read_conflict_stage`). `base` is `None` when stage 1
/// doesn't exist for this path (e.g. the file was added independently on
/// both sides).
///
/// Public (not just crate-internal) since `WorkspaceManager::conflict_stages`
/// hands this to callers outside this crate too -- e.g. pact-core's Arbiter
/// Write-fresh prompt (issue #106), which needs the same three-way content
/// this module's own resolvers already read.
pub struct ConflictStages {
    pub path: String,
    pub base: Option<(String, bool)>,
    pub ours: (String, bool),
    pub theirs: (String, bool),
}

pub(crate) struct ResolvedFile {
    pub(crate) content: String,
}

/// One semantic conflict resolution strategy. `can_handle` decides
/// applicability from the path alone, before any git stage is read, so
/// `try_auto_resolve` can pick a resolver without paying for stage reads on
/// every candidate. `resolve` does the actual merge and returns `Ok(None)`
/// for "understood the file type but couldn't safely resolve this specific
/// conflict" -- the same "fall through to a real conflict" contract every
/// caller of this trait already relies on.
pub(crate) trait SemanticResolver {
    fn can_handle(&self, path: &str) -> bool;
    fn resolve(&self, stages: &ConflictStages) -> Result<Option<ResolvedFile>>;
}

/// JSON-aware merge of `package.json`'s dependency blocks -- see DESIGN.md
/// ("pact-vcs > Semantic auto-resolution").
pub(crate) struct PackageJsonResolver;

impl SemanticResolver for PackageJsonResolver {
    fn can_handle(&self, path: &str) -> bool {
        Path::new(path).file_name().and_then(|n| n.to_str()) == Some("package.json")
    }

    fn resolve(&self, stages: &ConflictStages) -> Result<Option<ResolvedFile>> {
        let Some((base, _)) = &stages.base else {
            return Ok(None);
        };
        let (ours, ours_had_bom) = &stages.ours;
        let (theirs, _) = &stages.theirs;

        let (Ok(base), Ok(ours_value), Ok(theirs_value)) = (
            serde_json::from_str::<serde_json::Value>(base),
            serde_json::from_str::<serde_json::Value>(ours),
            serde_json::from_str::<serde_json::Value>(theirs),
        ) else {
            return Ok(None);
        };

        let mut ours_stripped = ours_value.clone();
        let mut theirs_stripped = theirs_value.clone();
        if let (Some(o), Some(t)) = (ours_stripped.as_object_mut(), theirs_stripped.as_object_mut()) {
            for key in PACKAGE_JSON_DEP_KEYS {
                o.remove(*key);
                t.remove(*key);
            }
        }
        if ours_stripped != theirs_stripped {
            return Ok(None);
        }

        let Some(mut merged_obj) = ours_value.as_object().cloned() else {
            return Ok(None);
        };

        for key in PACKAGE_JSON_DEP_KEYS {
            let base_block = base.get(*key).and_then(|v| v.as_object());
            let ours_block = ours_value.get(*key).and_then(|v| v.as_object());
            let theirs_block = theirs_value.get(*key).and_then(|v| v.as_object());
            if base_block.is_none() && ours_block.is_none() && theirs_block.is_none() {
                continue;
            }

            let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            if let Some(m) = ours_block {
                names.extend(m.keys().cloned());
            }
            if let Some(m) = theirs_block {
                names.extend(m.keys().cloned());
            }

            let mut merged_block = serde_json::Map::new();
            for name in names {
                let base_v = base_block.and_then(|m| m.get(&name));
                let ours_v = ours_block.and_then(|m| m.get(&name));
                let theirs_v = theirs_block.and_then(|m| m.get(&name));
                let resolved = match (ours_v, theirs_v) {
                    (Some(o), Some(t)) if o == t => o.clone(),
                    (Some(o), Some(t)) => {
                        if base_v == Some(o) {
                            t.clone() // only theirs changed this dependency
                        } else if base_v == Some(t) {
                            o.clone() // only ours changed this dependency
                        } else {
                            return Ok(None); // both changed it, differently
                        }
                    }
                    (Some(o), None) => o.clone(),
                    (None, Some(t)) => t.clone(),
                    (None, None) => unreachable!("name came from ours_block or theirs_block"),
                };
                merged_block.insert(name, resolved);
            }
            merged_obj.insert(key.to_string(), serde_json::Value::Object(merged_block));
        }

        let merged_value = serde_json::Value::Object(merged_obj);

        // `to_string_pretty` alone would do two things this resolver isn't
        // supposed to do: reorder every top-level key alphabetically
        // (serde_json's `Value::Object` is a plain `serde_json::Map`, which
        // without the `preserve_order` feature is BTreeMap-backed) and
        // hardcode 2-space indent regardless of the file's own convention.
        // `merged_obj` above is built by cloning `ours_value`'s object and
        // updating entries in place, so with `preserve_order` on, its key
        // order already matches "ours" -- this only needs to match the
        // indent width, not touch ordering.
        let indent = super::detect_json_indent(ours);
        let mut buf = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(&indent);
        let mut serializer = serde_json::Serializer::with_formatter(&mut buf, formatter);
        serde::Serialize::serialize(&merged_value, &mut serializer)
            .context("serializing auto-resolved package.json")?;
        let mut result = String::from_utf8(buf).context("auto-resolved package.json was not valid UTF-8")?;
        result.push('\n');

        // "ours" is the integration branch's existing convention (same
        // reasoning `detect_json_indent(ours)` above already uses for
        // indent width) -- if its committed package.json had a BOM,
        // restore it here. Otherwise the merged output silently drops it,
        // even though nothing about resolving the dependency-block
        // conflict was ever meant to change the file's encoding (issue
        // #79).
        if *ours_had_bom {
            result.insert(0, '\u{FEFF}');
        }

        Ok(Some(ResolvedFile { content: result }))
    }
}

const PACKAGE_JSON_DEP_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

/// Plain line-union merge for a `--append-only`-matched file -- see
/// DESIGN.md ("pact-vcs > Semantic auto-resolution"). Configured per call
/// with the glob patterns from `merge-all --append-only`, since (unlike
/// `PackageJsonResolver`) which files this applies to isn't fixed -- it's
/// whatever the caller named.
pub(crate) struct UnionResolver {
    pub(crate) globs: Vec<String>,
}

impl SemanticResolver for UnionResolver {
    fn can_handle(&self, path: &str) -> bool {
        self.globs.iter().any(|pattern| glob_matches(pattern, path))
    }

    fn resolve(&self, stages: &ConflictStages) -> Result<Option<ResolvedFile>> {
        let (ours, _) = &stages.ours;
        let (theirs, _) = &stages.theirs;

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut merged_lines: Vec<&str> = Vec::new();
        for line in ours.lines() {
            if seen.insert(line) {
                merged_lines.push(line);
            }
        }
        for line in theirs.lines() {
            if seen.insert(line) {
                merged_lines.push(line);
            }
        }

        let mut result = merged_lines.join("\n");
        result.push('\n');

        // A plain line-concat is wrong for any file with "final
        // assignment/declaration wins" semantics: two independent barrel
        // appends can each be a no-conflict-looking line, yet together
        // produce two `module.exports =` statements (second silently wins,
        // first is dropped) or two declarations binding the same
        // identifier (a real redeclaration SyntaxError in JS/TS). Confirmed
        // by hand: this exact shape reliably breaks a merged CommonJS
        // barrel. Treat that as "don't understand this well enough to
        // auto-resolve" rather than reporting a broken merge as a success.
        if !union_merge_is_safe(&stages.path, &result) {
            return Ok(None);
        }

        Ok(Some(ResolvedFile { content: result }))
    }
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .ok()
        .map(|g| g.compile_matcher().is_match(path))
        .unwrap_or(false)
}

/// File extensions `UnionResolver`'s safety check applies to.
const UNION_SAFETY_CHECKED_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "tsx"];

/// Heuristic (not a real parser) check for the two `--append-only` failure
/// modes found in practice on JS/TS files: two `module.exports =` /
/// `export default` statements surviving into the same merged file, and two
/// declarations binding the same identifier in the same scope. False
/// negatives are possible by design (this is intentionally cheap, not a
/// full parser); a false positive just means a file that would otherwise
/// silently break instead falls through to "needs a human", which is the
/// safe direction. Non-JS/TS files are never checked.
fn union_merge_is_safe(file: &str, content: &str) -> bool {
    let ext = Path::new(file).extension().and_then(|e| e.to_str()).unwrap_or("");
    if !UNION_SAFETY_CHECKED_EXTENSIONS.contains(&ext) {
        return true;
    }

    let mut module_exports_count = 0u32;
    let mut default_export_count = 0u32;
    let mut bound_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        if let Some(rest) = line.strip_prefix("module.exports") {
            let rest = rest.trim_start();
            if rest.starts_with('=') && !rest.starts_with("==") {
                module_exports_count += 1;
            }
        }
        if line.starts_with("export default ") || line == "export default" || line == "export default;" {
            default_export_count += 1;
        }

        for keyword in ["const ", "let ", "var "] {
            if let Some(rest) = line.strip_prefix(keyword) {
                for name in binding_names(rest) {
                    if !bound_names.insert(name) {
                        return false;
                    }
                }
            }
        }
    }

    module_exports_count <= 1 && default_export_count <= 1
}

/// Extracts the identifier(s) a single `const`/`let`/`var` declaration
/// binds, from the source text right after the keyword -- handles a plain
/// identifier (`x = ...`), object destructuring (`{ a, b: c, ...rest } =
/// ...`), and array destructuring (`[a, , b] = ...`). Best-effort: only
/// needs to catch the common barrel-export shape, not be a full parser.
fn binding_names(rest: &str) -> Vec<String> {
    let rest = rest.trim_start();
    let extract = |inner: &str| -> Vec<String> {
        inner
            .split(',')
            .filter_map(|entry| {
                let entry = entry.trim().trim_start_matches("...").trim();
                let key = entry.split(':').next().unwrap_or(entry).trim();
                let key = key.split('=').next().unwrap_or(key).trim();
                if key.is_empty() {
                    None
                } else {
                    Some(key.to_string())
                }
            })
            .collect()
    };

    if let Some(inner) = rest.strip_prefix('{') {
        match inner.find('}') {
            Some(end) => extract(&inner[..end]),
            None => Vec::new(),
        }
    } else if let Some(inner) = rest.strip_prefix('[') {
        match inner.find(']') {
            Some(end) => extract(&inner[..end]),
            None => Vec::new(),
        }
    } else {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        if name.is_empty() {
            Vec::new()
        } else {
            vec![name]
        }
    }
}

/// The full set of semantic resolvers, in priority order -- `try_auto_resolve`
/// picks the first whose `can_handle` matches. `union_globs` comes from
/// `merge-all --append-only`; empty when the caller didn't pass any.
pub(crate) fn resolvers(union_globs: &[String]) -> Vec<Box<dyn SemanticResolver>> {
    vec![
        Box::new(PackageJsonResolver),
        Box::new(UnionResolver { globs: union_globs.to_vec() }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_merge_is_safe_ignores_non_js_ts_files() {
        // Two "final value wins" assignments, but this isn't a checked
        // extension, so the safety check doesn't apply.
        let content = "module.exports = { a };\nmodule.exports = { b };\n";
        assert!(union_merge_is_safe("CHANGELOG.md", content));
    }

    #[test]
    fn union_merge_is_safe_accepts_plain_barrel_append() {
        let content = "export {};\nexport * from './chunk';\nexport * from './omit';\n";
        assert!(union_merge_is_safe("src/barrel.ts", content));
    }

    #[test]
    fn union_merge_rejects_duplicate_module_exports() {
        let content = "const { mul } = require('./mul');\n\
                        const { div } = require('./div');\n\
                        module.exports = { mul };\n\
                        module.exports = { div };\n";
        assert!(!union_merge_is_safe("src/index.js", content));
    }

    #[test]
    fn union_merge_rejects_redeclared_destructured_binding() {
        let content = "const { add, sub, mul } = require('../src/index');\n\
                        const { add, sub, div } = require('../src/index');\n";
        assert!(!union_merge_is_safe("test/index.test.js", content));
    }

    #[test]
    fn union_merge_rejects_duplicate_export_default() {
        let content = "export default class A {}\nexport default class B {}\n";
        assert!(!union_merge_is_safe("src/widget.tsx", content));
    }

    #[test]
    fn union_merge_allows_module_exports_property_assignment() {
        // `module.exports.foo = ...` is not a full reassignment, so two of
        // these (for different properties) is a legitimate union merge.
        let content = "module.exports.mul = require('./mul');\nmodule.exports.div = require('./div');\n";
        assert!(union_merge_is_safe("src/index.js", content));
    }

    #[test]
    fn package_json_resolver_can_handle_matches_only_the_exact_filename() {
        let resolver = PackageJsonResolver;
        assert!(resolver.can_handle("package.json"));
        assert!(resolver.can_handle("nested/dir/package.json"));
        assert!(!resolver.can_handle("package-lock.json"));
        assert!(!resolver.can_handle("src/package.json.bak"));
    }

    #[test]
    fn union_resolver_can_handle_respects_configured_globs() {
        let resolver = UnionResolver { globs: vec!["src/barrel.ts".to_string()] };
        assert!(resolver.can_handle("src/barrel.ts"));
        assert!(!resolver.can_handle("src/other.ts"));
    }

    #[test]
    fn package_json_resolver_merges_non_conflicting_dependency_additions() {
        let stages = ConflictStages {
            path: "package.json".to_string(),
            base: Some(("{\"name\":\"x\",\"dependencies\":{}}".to_string(), false)),
            ours: ("{\"name\":\"x\",\"dependencies\":{\"a\":\"1.0.0\"}}".to_string(), false),
            theirs: ("{\"name\":\"x\",\"dependencies\":{\"b\":\"2.0.0\"}}".to_string(), false),
        };
        let resolved = PackageJsonResolver.resolve(&stages).unwrap().unwrap();
        assert!(resolved.content.contains("\"a\": \"1.0.0\""));
        assert!(resolved.content.contains("\"b\": \"2.0.0\""));
    }

    #[test]
    fn package_json_resolver_gives_up_when_both_sides_change_the_same_dependency() {
        let stages = ConflictStages {
            path: "package.json".to_string(),
            base: Some(("{\"name\":\"x\",\"dependencies\":{\"a\":\"1.0.0\"}}".to_string(), false)),
            ours: ("{\"name\":\"x\",\"dependencies\":{\"a\":\"2.0.0\"}}".to_string(), false),
            theirs: ("{\"name\":\"x\",\"dependencies\":{\"a\":\"3.0.0\"}}".to_string(), false),
        };
        assert!(PackageJsonResolver.resolve(&stages).unwrap().is_none());
    }

    #[test]
    fn union_resolver_merges_distinct_lines_from_both_sides() {
        let stages = ConflictStages {
            path: "src/barrel.ts".to_string(),
            base: None,
            ours: ("export {};\nexport * from './chunk';\n".to_string(), false),
            theirs: ("export {};\nexport * from './omit';\n".to_string(), false),
        };
        let resolved = UnionResolver { globs: vec![] }.resolve(&stages).unwrap().unwrap();
        assert!(resolved.content.contains("export * from './chunk';"));
        assert!(resolved.content.contains("export * from './omit';"));
    }

    #[test]
    fn union_resolver_declines_an_unsafe_merge() {
        let stages = ConflictStages {
            path: "src/index.js".to_string(),
            base: None,
            ours: ("module.exports = { a };\n".to_string(), false),
            theirs: ("module.exports = { b };\n".to_string(), false),
        };
        assert!(UnionResolver { globs: vec![] }.resolve(&stages).unwrap().is_none());
    }
}
