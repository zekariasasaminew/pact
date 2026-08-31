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

/// TOML-aware merge of dependency tables in `Cargo.toml`/`pyproject.toml`
/// -- the same shape as `PackageJsonResolver`, generalized to (a) a
/// dotted path instead of a flat top-level key, since Poetry's
/// dependency tables live under `[tool.poetry...]`, and (b) `toml_edit`
/// for the actual write instead of a whole-document reserialize. TOML's
/// comment culture is real (Cargo.toml routinely has per-dependency
/// `# why this is pinned` comments) in a way JSON's isn't, so unlike
/// `PackageJsonResolver` -- which accepts losing exact formatting
/// details on every resolved file, matching JSON's own conventions --
/// this only touches the specific table entries that actually changed,
/// leaving every comment, unrelated key, and unrelated table byte-for-byte
/// as "ours" had it. See DESIGN.md ("pact-vcs > TOML manifest structural
/// merge (issue #272)").
pub(crate) struct TomlManifestResolver {
    pub(crate) file_name: &'static str,
    /// Each entry is a dotted path to one dependency table, e.g.
    /// `&["dependencies"]` for Cargo.toml's top-level table, or
    /// `&["tool", "poetry", "dependencies"]` for Poetry's.
    pub(crate) dependency_paths: &'static [&'static [&'static str]],
}

impl SemanticResolver for TomlManifestResolver {
    fn can_handle(&self, path: &str) -> bool {
        Path::new(path).file_name().and_then(|n| n.to_str()) == Some(self.file_name)
    }

    fn resolve(&self, stages: &ConflictStages) -> Result<Option<ResolvedFile>> {
        let Some((base, _)) = &stages.base else {
            return Ok(None);
        };
        let (ours, ours_had_bom) = &stages.ours;
        let (theirs, _) = &stages.theirs;

        // Parsed with the plain `toml` crate, semantic-value-only (no
        // formatting/decor) -- used for both the "everything outside the
        // dependency tables is identical" safety gate and the per-name
        // 3-way resolution below. `toml_edit::Value` deliberately doesn't
        // implement `PartialEq` (only `Debug`/`Clone` -- confirmed
        // against its own source), so this crate is the only sound way
        // to compare TOML content by value rather than by formatting.
        let (Ok(base_v), Ok(ours_v), Ok(theirs_v)) = (
            toml::from_str::<toml::Value>(base),
            toml::from_str::<toml::Value>(ours),
            toml::from_str::<toml::Value>(theirs),
        ) else {
            return Ok(None);
        };

        let mut ours_stripped = ours_v.clone();
        let mut theirs_stripped = theirs_v.clone();
        for path in self.dependency_paths {
            remove_toml_path(&mut ours_stripped, path);
            remove_toml_path(&mut theirs_stripped, path);
        }
        if ours_stripped != theirs_stripped {
            return Ok(None);
        }

        let Ok(mut doc) = ours.parse::<toml_edit::DocumentMut>() else {
            return Ok(None);
        };

        for path in self.dependency_paths {
            let base_block = toml_path_table(&base_v, path);
            let ours_block = toml_path_table(&ours_v, path);
            let theirs_block = toml_path_table(&theirs_v, path);
            if base_block.is_none() && ours_block.is_none() && theirs_block.is_none() {
                continue;
            }

            let mut names: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
            if let Some(m) = ours_block {
                names.extend(m.keys());
            }
            if let Some(m) = theirs_block {
                names.extend(m.keys());
            }

            let Some(edit_table) = toml_edit_path_table_mut(&mut doc, path) else {
                return Ok(None);
            };

            for name in names {
                let base_val = base_block.and_then(|m| m.get(name));
                let ours_val = ours_block.and_then(|m| m.get(name));
                let theirs_val = theirs_block.and_then(|m| m.get(name));
                let resolved = match (ours_val, theirs_val) {
                    (Some(o), Some(t)) if o == t => continue, // already what "ours" has, don't touch its formatting
                    (Some(o), Some(t)) => {
                        if base_val == Some(o) {
                            t.clone() // only theirs changed this dependency
                        } else if base_val == Some(t) {
                            continue // only ours changed it -- already correct, don't touch it
                        } else {
                            return Ok(None); // both changed it, differently
                        }
                    }
                    (Some(_), None) => continue, // ours already has it, theirs never touched this table
                    (None, Some(t)) => t.clone(),
                    (None, None) => unreachable!("name came from ours_block or theirs_block"),
                };
                let Some(edit_value) = toml_value_to_edit(&resolved) else {
                    return Ok(None); // an unsupported value shape (e.g. a datetime) -- fall through to a real conflict
                };
                edit_table[name.as_str()] = toml_edit::Item::Value(edit_value);
            }
        }

        let mut result = doc.to_string();
        if !result.ends_with('\n') {
            result.push('\n');
        }
        if *ours_had_bom {
            result.insert(0, '\u{FEFF}');
        }

        Ok(Some(ResolvedFile { content: result }))
    }
}

/// Removes the table at a dotted path from a `toml::Value`, if present --
/// used to build the "everything besides the dependency tables" copy for
/// `TomlManifestResolver`'s safety gate. A no-op if any segment along the
/// path isn't a table (nothing to strip).
fn remove_toml_path(value: &mut toml::Value, path: &[&str]) {
    let Some((last, ancestors)) = path.split_last() else { return };
    let mut current = value;
    for segment in ancestors {
        let Some(next) = current.get_mut(*segment) else { return };
        current = next;
    }
    if let Some(table) = current.as_table_mut() {
        table.remove(*last);
    }
}

/// Reads the table at a dotted path from a `toml::Value`, if every
/// segment along the way is itself a table.
fn toml_path_table<'a>(value: &'a toml::Value, path: &[&str]) -> Option<&'a toml::map::Map<String, toml::Value>> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_table()
}

/// Same traversal as `toml_path_table`, but over a `toml_edit::DocumentMut`
/// being edited -- creates any missing intermediate table along the path
/// (e.g. a workspace that never had a `[dependencies]` table at all until
/// this merge introduces one) rather than failing, since an entirely
/// absent table is a legitimate starting point, not a malformed one.
fn toml_edit_path_table_mut<'a>(doc: &'a mut toml_edit::DocumentMut, path: &[&str]) -> Option<&'a mut toml_edit::Table> {
    let mut current = doc.as_table_mut();
    for segment in path {
        if !current.contains_key(segment) {
            current.insert(segment, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        current = current.get_mut(segment)?.as_table_mut()?;
    }
    Some(current)
}

/// Converts a semantic `toml::Value` (from the `toml` crate) into a
/// `toml_edit::Value` (from the `toml_edit` crate) for insertion into an
/// edited document -- the two crates have no conversion between their
/// value types, since one is plain-serde-backed and the other carries
/// formatting. Returns `None` for `Datetime` (a dependency-table value
/// is never realistically a datetime; falling through to a real conflict
/// is safer than guessing at a conversion never exercised in practice).
fn toml_value_to_edit(value: &toml::Value) -> Option<toml_edit::Value> {
    match value {
        toml::Value::String(s) => Some(toml_edit::Value::from(s.as_str())),
        toml::Value::Integer(i) => Some(toml_edit::Value::from(*i)),
        toml::Value::Float(f) => Some(toml_edit::Value::from(*f)),
        toml::Value::Boolean(b) => Some(toml_edit::Value::from(*b)),
        toml::Value::Datetime(_) => None,
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(toml_value_to_edit(item)?);
            }
            Some(toml_edit::Value::Array(array))
        }
        toml::Value::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, v) in table {
                inline.insert(k, toml_value_to_edit(v)?);
            }
            Some(toml_edit::Value::InlineTable(inline))
        }
    }
}

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

        // Sentinel-marker insertion mode (issue #87): a plain append at
        // file end loses position for a barrel file whose union-mergeable
        // block sits above other code (a finalizer like `start()`, or a
        // trailing `module.exports`) -- only the first workspace's
        // addition ever lands inside the intended block; every merge
        // after that appends past the finalizer instead. Opt-in via a
        // marker pair the user adds to their own file -- no CLI flag,
        // the markers' presence in `ours` *is* the opt-in. Exactly one
        // well-formed pair activates it; zero markers keeps today's
        // plain-append behavior unchanged (non-breaking); anything else
        // (multiple pairs, unmatched start/end) refuses rather than
        // guessing which pair is the right insertion point.
        //
        // Scanned against `ours`' *raw* lines, not the deduped list built
        // below -- two marker pairs using byte-identical marker text would
        // otherwise silently collapse into what looks like one pair once
        // the dedup step below removes the repeated line, defeating the
        // refusal this mode is supposed to give a genuinely ambiguous file.
        let ours_lines: Vec<&str> = ours.lines().collect();
        let end_marker_text = match sentinel_end_marker_index(&ours_lines) {
            MarkerScan::NoMarkers => None,
            MarkerScan::OnePair(end_index) => Some(ours_lines[end_index]),
            MarkerScan::Malformed => return Ok(None),
        };

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut merged_lines: Vec<&str> = Vec::new();
        for line in ours.lines() {
            if seen.insert(line) {
                merged_lines.push(line);
            }
        }

        // The end marker's index within the *deduped* list -- found by
        // content, not carried over from `ours_lines`' index, since an
        // earlier duplicate line elsewhere in `ours` could have shifted it.
        let insertion_index = end_marker_text.and_then(|text| merged_lines.iter().position(|l| *l == text));

        match insertion_index {
            Some(mut index) => {
                for line in theirs.lines() {
                    if is_sentinel_marker_line(line) {
                        // Never insert a duplicate marker line, even one
                        // that happens to differ in surrounding comment
                        // syntax from ours' own -- dedup below only
                        // catches an exact text match.
                        continue;
                    }
                    if seen.insert(line) {
                        merged_lines.insert(index, line);
                        index += 1;
                    }
                }
            }
            None => {
                for line in theirs.lines() {
                    if seen.insert(line) {
                        merged_lines.push(line);
                    }
                }
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

/// The literal tokens a sentinel marker line contains -- deliberately not
/// tied to any one comment syntax (`//`, `#`, `<!--`, ...). This feature
/// sits on `--append-only`, which is itself language-agnostic (any glob,
/// any file type); hardcoding a `//`-style marker would silently not
/// work for a Python/YAML/HTML barrel file. A caller writes whatever
/// comment syntax is idiomatic for their language, as long as the line
/// contains this token -- `// pact:union-start`, `# pact:union-start`,
/// `<!-- pact:union-start -->` all match.
const SENTINEL_START_TOKEN: &str = "pact:union-start";
const SENTINEL_END_TOKEN: &str = "pact:union-end";

fn is_sentinel_marker_line(line: &str) -> bool {
    line.contains(SENTINEL_START_TOKEN) || line.contains(SENTINEL_END_TOKEN)
}

enum MarkerScan {
    /// No start or end marker anywhere -- today's plain-append behavior
    /// applies unchanged.
    NoMarkers,
    /// Exactly one well-formed pair (one start, one end, start before
    /// end) -- the end marker's index in `lines`.
    OnePair(usize),
    /// Anything else (multiple pairs, an end with no start, a start
    /// after its end, ...) -- refuse rather than guess which pair, if
    /// any, is the right insertion point.
    Malformed,
}

fn sentinel_end_marker_index(lines: &[&str]) -> MarkerScan {
    let starts: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.contains(SENTINEL_START_TOKEN)).map(|(i, _)| i).collect();
    let ends: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.contains(SENTINEL_END_TOKEN)).map(|(i, _)| i).collect();

    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => MarkerScan::NoMarkers,
        (&[start], &[end]) if start < end => MarkerScan::OnePair(end),
        _ => MarkerScan::Malformed,
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
        Box::new(cargo_toml_resolver()),
        Box::new(pyproject_toml_resolver()),
        Box::new(UnionResolver { globs: union_globs.to_vec() }),
    ]
}

fn cargo_toml_resolver() -> TomlManifestResolver {
    TomlManifestResolver {
        file_name: "Cargo.toml",
        dependency_paths: &[&["dependencies"], &["dev-dependencies"], &["build-dependencies"]],
    }
}

/// Poetry's dependency tables only -- see DESIGN.md ("pact-vcs > TOML
/// manifest structural merge (issue #272)") for why PEP 621's
/// `[project.dependencies]` (a flat array of version-spec strings, not a
/// name-keyed table) is deliberately out of scope for this resolver: it
/// isn't the same shape `PackageJsonResolver`/`TomlManifestResolver`
/// already handle, it's a different merge problem (array-union keyed by
/// the package name inside each string).
fn pyproject_toml_resolver() -> TomlManifestResolver {
    TomlManifestResolver {
        file_name: "pyproject.toml",
        dependency_paths: &[
            &["tool", "poetry", "dependencies"],
            &["tool", "poetry", "dev-dependencies"],
            &["tool", "poetry", "group", "dev", "dependencies"],
        ],
    }
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

    fn union_stages(ours: &str, theirs: &str) -> ConflictStages {
        ConflictStages {
            path: "src/plugins.js".to_string(),
            base: None,
            ours: (ours.to_string(), false),
            theirs: (theirs.to_string(), false),
        }
    }

    #[test]
    fn sentinel_end_marker_index_finds_a_well_formed_pair() {
        let lines = ["a", "// pact:union-start", "b", "// pact:union-end", "c"];
        assert!(matches!(sentinel_end_marker_index(&lines), MarkerScan::OnePair(3)));
    }

    #[test]
    fn sentinel_end_marker_index_is_none_with_no_markers() {
        let lines = ["a", "b", "c"];
        assert!(matches!(sentinel_end_marker_index(&lines), MarkerScan::NoMarkers));
    }

    #[test]
    fn sentinel_end_marker_index_refuses_multiple_pairs() {
        let lines = ["// pact:union-start", "a", "// pact:union-end", "// pact:union-start", "b", "// pact:union-end"];
        assert!(matches!(sentinel_end_marker_index(&lines), MarkerScan::Malformed));
    }

    #[test]
    fn sentinel_end_marker_index_refuses_an_end_before_its_start() {
        let lines = ["// pact:union-end", "a", "// pact:union-start"];
        assert!(matches!(sentinel_end_marker_index(&lines), MarkerScan::Malformed));
    }

    #[test]
    fn sentinel_end_marker_index_refuses_an_unmatched_start() {
        let lines = ["// pact:union-start", "a"];
        assert!(matches!(sentinel_end_marker_index(&lines), MarkerScan::Malformed));
    }

    #[test]
    fn union_resolve_falls_back_to_append_at_end_with_no_markers() {
        let resolver = UnionResolver { globs: vec!["src/*.js".to_string()] };
        let stages = union_stages(
            "register('a');\nfinalize();\n",
            "register('b');\nfinalize();\n",
        );
        let resolved = resolver.resolve(&stages).unwrap().unwrap();
        assert_eq!(resolved.content, "register('a');\nfinalize();\nregister('b');\n");
    }

    #[test]
    fn union_resolve_inserts_before_the_end_marker_when_one_pair_is_present() {
        let resolver = UnionResolver { globs: vec!["src/*.js".to_string()] };
        let stages = union_stages(
            "// pact:union-start\nregister('a');\n// pact:union-end\nfinalize();\n",
            "// pact:union-start\nregister('a');\nregister('b');\n// pact:union-end\nfinalize();\n",
        );
        let resolved = resolver.resolve(&stages).unwrap().unwrap();
        assert_eq!(
            resolved.content,
            "// pact:union-start\nregister('a');\nregister('b');\n// pact:union-end\nfinalize();\n",
            "expected register('b') inserted before the end marker, not after finalize()"
        );
    }

    #[test]
    fn union_resolve_accumulates_multiple_insertions_in_order_before_the_marker() {
        // Simulates a second sequential merge against an already-merged
        // `ours` that itself resulted from a prior sentinel insertion --
        // confirms insertions compound correctly across N workspaces.
        let resolver = UnionResolver { globs: vec!["src/*.js".to_string()] };
        let stages = union_stages(
            "// pact:union-start\nregister('a');\nregister('b');\n// pact:union-end\nfinalize();\n",
            "// pact:union-start\nregister('a');\nregister('c');\n// pact:union-end\nfinalize();\n",
        );
        let resolved = resolver.resolve(&stages).unwrap().unwrap();
        assert_eq!(
            resolved.content,
            "// pact:union-start\nregister('a');\nregister('b');\nregister('c');\n// pact:union-end\nfinalize();\n"
        );
    }

    #[test]
    fn union_resolve_never_duplicates_theirs_own_marker_lines() {
        let resolver = UnionResolver { globs: vec!["src/*.js".to_string()] };
        let stages = union_stages(
            "// pact:union-start\nregister('a');\n// pact:union-end\nfinalize();\n",
            "// pact:union-start\nregister('a');\nregister('b');\n// pact:union-end\nfinalize();\n",
        );
        let resolved = resolver.resolve(&stages).unwrap().unwrap();
        assert_eq!(
            resolved.content.matches("pact:union-start").count(),
            1,
            "theirs' own copy of the start marker must not be inserted as a duplicate line"
        );
        assert_eq!(resolved.content.matches("pact:union-end").count(), 1);
    }

    #[test]
    fn union_resolve_refuses_when_ours_has_multiple_marker_pairs() {
        let resolver = UnionResolver { globs: vec!["src/*.js".to_string()] };
        let stages = union_stages(
            "// pact:union-start\na\n// pact:union-end\n// pact:union-start\nb\n// pact:union-end\n",
            "c\n",
        );
        assert!(resolver.resolve(&stages).unwrap().is_none(), "expected a refusal (real conflict), not a guess");
    }

    #[test]
    fn union_resolve_marker_detection_is_comment_syntax_agnostic() {
        // Python-style `#` comments -- the whole point of matching on the
        // literal token rather than a fixed `//` prefix.
        let resolver = UnionResolver { globs: vec!["src/*.py".to_string()] };
        let mut stages = union_stages(
            "# pact:union-start\nregister('a')\n# pact:union-end\nfinalize()\n",
            "# pact:union-start\nregister('a')\nregister('b')\n# pact:union-end\nfinalize()\n",
        );
        stages.path = "src/plugins.py".to_string();
        let resolved = resolver.resolve(&stages).unwrap().unwrap();
        assert_eq!(
            resolved.content,
            "# pact:union-start\nregister('a')\nregister('b')\n# pact:union-end\nfinalize()\n"
        );
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
    fn cargo_toml_resolver_can_handle_matches_only_the_exact_filename() {
        let resolver = cargo_toml_resolver();
        assert!(resolver.can_handle("Cargo.toml"));
        assert!(resolver.can_handle("crates/pact-vcs/Cargo.toml"));
        assert!(!resolver.can_handle("Cargo.lock"));
    }

    #[test]
    fn cargo_toml_resolver_merges_non_conflicting_dependency_additions_and_preserves_comments() {
        let stages = ConflictStages {
            path: "Cargo.toml".to_string(),
            base: Some((
                "[package]\nname = \"x\"\n\n[dependencies]\n# pinned, see issue #1\nserde = \"1\"\n".to_string(),
                false,
            )),
            ours: (
                "[package]\nname = \"x\"\n\n[dependencies]\n# pinned, see issue #1\nserde = \"1\"\nanyhow = \"1\"\n"
                    .to_string(),
                false,
            ),
            theirs: (
                "[package]\nname = \"x\"\n\n[dependencies]\n# pinned, see issue #1\nserde = \"1\"\nuuid = \"1\"\n"
                    .to_string(),
                false,
            ),
        };
        let resolved = cargo_toml_resolver().resolve(&stages).unwrap().unwrap();
        assert!(resolved.content.contains("anyhow = \"1\""), "got: {}", resolved.content);
        assert!(resolved.content.contains("uuid = \"1\""), "got: {}", resolved.content);
        assert!(
            resolved.content.contains("# pinned, see issue #1"),
            "expected the unrelated comment to survive the merge untouched, got: {}",
            resolved.content
        );
    }

    #[test]
    fn cargo_toml_resolver_gives_up_when_both_sides_change_the_same_dependency() {
        let stages = ConflictStages {
            path: "Cargo.toml".to_string(),
            base: Some(("[dependencies]\nserde = \"1\"\n".to_string(), false)),
            ours: ("[dependencies]\nserde = \"2\"\n".to_string(), false),
            theirs: ("[dependencies]\nserde = \"3\"\n".to_string(), false),
        };
        assert!(cargo_toml_resolver().resolve(&stages).unwrap().is_none());
    }

    #[test]
    fn cargo_toml_resolver_gives_up_when_something_outside_dependencies_also_changed() {
        let stages = ConflictStages {
            path: "Cargo.toml".to_string(),
            base: Some(("[package]\nversion = \"0.1.0\"\n\n[dependencies]\n".to_string(), false)),
            ours: ("[package]\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n".to_string(), false),
            theirs: ("[package]\nversion = \"0.2.0\"\n\n[dependencies]\n".to_string(), false),
        };
        assert!(
            cargo_toml_resolver().resolve(&stages).unwrap().is_none(),
            "a real conflict outside the dependency tables must not be silently dropped"
        );
    }

    #[test]
    fn cargo_toml_resolver_handles_an_inline_table_dependency_value() {
        let stages = ConflictStages {
            path: "Cargo.toml".to_string(),
            base: Some(("[dependencies]\n".to_string(), false)),
            ours: ("[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n".to_string(), false),
            theirs: ("[dependencies]\nuuid = { version = \"1\", features = [\"v4\"] }\n".to_string(), false),
        };
        let resolved = cargo_toml_resolver().resolve(&stages).unwrap().unwrap();
        assert!(resolved.content.contains("features"), "got: {}", resolved.content);
        assert!(resolved.content.contains("uuid"), "got: {}", resolved.content);
    }

    #[test]
    fn pyproject_toml_resolver_merges_non_conflicting_poetry_dependency_additions() {
        let stages = ConflictStages {
            path: "pyproject.toml".to_string(),
            base: Some(("[tool.poetry.dependencies]\npython = \"^3.11\"\n".to_string(), false)),
            ours: (
                "[tool.poetry.dependencies]\npython = \"^3.11\"\nrequests = \"^2.0\"\n".to_string(),
                false,
            ),
            theirs: ("[tool.poetry.dependencies]\npython = \"^3.11\"\nclick = \"^8.0\"\n".to_string(), false),
        };
        let resolved = pyproject_toml_resolver().resolve(&stages).unwrap().unwrap();
        assert!(resolved.content.contains("requests"), "got: {}", resolved.content);
        assert!(resolved.content.contains("click"), "got: {}", resolved.content);
    }

    #[test]
    fn pyproject_toml_resolver_can_handle_matches_only_the_exact_filename() {
        let resolver = pyproject_toml_resolver();
        assert!(resolver.can_handle("pyproject.toml"));
        assert!(!resolver.can_handle("poetry.lock"));
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
