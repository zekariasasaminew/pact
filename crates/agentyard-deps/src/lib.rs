//! Phase 1 (not yet implemented): the dependency broker.
//!
//! Detects a project's package manager(s) via marker files and either
//! passes through to that ecosystem's already-shared global cache (pnpm,
//! uv, Cargo, Go modules, Maven all have one) or, for ecosystems without one
//! (plain npm, plain pip/venv), materializes a lockfile-hash-keyed
//! content-addressed store into the new workspace via hardlink, falling
//! back to reflink then plain copy when hardlinking isn't possible (e.g.
//! across filesystems).
