use std::path::Path;

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Repo-local defaults for `--agent`/`--safety`, read from `pact.toml` at
/// the repo root -- see DESIGN.md ("pact-cli > `pact.toml` / `pact init`").
/// A CLI flag always wins when both are given; this only fills in what the
/// flag would otherwise have defaulted to.
#[derive(Debug, Default, Deserialize)]
pub struct PactConfig {
    #[serde(default)]
    defaults: Defaults,
    /// `[tiers.<name>]`: agent name to model id (issue #387).
    #[serde(default)]
    tiers: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
struct Defaults {
    agent: Option<String>,
    safety: Option<String>,
    deps: Option<String>,
    max_concurrent: Option<usize>,
    min_free_mem_mb: Option<u64>,
    stagger_ms: Option<u64>,
    per_lane_reserve_mb: Option<u64>,
    runtime: Option<String>,
    model: Option<String>,
    #[serde(default)]
    prepare: Vec<String>,
}

impl PactConfig {
    pub const FILE_NAME: &'static str = "pact.toml";

    /// Reads `pact.toml` from `repo_root` if it exists. A missing file is
    /// not an error -- pact works fully without one, this is purely an
    /// opt-in convenience -- but a present, malformed one is, so a typo
    /// doesn't silently fall back to "no config" instead of being reported.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(Self::FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context(format!("failed to read {}", path.display())),
        }
    }

    pub fn default_agent(&self) -> Option<&str> {
        self.defaults.agent.as_deref()
    }

    pub fn default_safety(&self) -> Option<&str> {
        self.defaults.safety.as_deref()
    }

    /// `defaults.deps`: the `--deps` mode to use when the flag is omitted
    /// (issue #283). Validated at use, not load, so an unknown value is
    /// reported against the flag it stands in for.
    pub fn default_deps(&self) -> Option<&str> {
        self.defaults.deps.as_deref()
    }

    /// `defaults.max_concurrent`/`min_free_mem_mb`/`stagger_ms`: the
    /// `spawn-many` admission policy when the flags are omitted (issue
    /// #285).
    pub fn default_max_concurrent(&self) -> Option<usize> {
        self.defaults.max_concurrent
    }

    pub fn default_min_free_mem_mb(&self) -> Option<u64> {
        self.defaults.min_free_mem_mb
    }

    pub fn default_stagger_ms(&self) -> Option<u64> {
        self.defaults.stagger_ms
    }

    /// `defaults.per_lane_reserve_mb`: memory each running lane is
    /// reserved to grow into when the next admission is decided (issue
    /// #320).
    pub fn default_per_lane_reserve_mb(&self) -> Option<u64> {
        self.defaults.per_lane_reserve_mb
    }

    /// `defaults.runtime`: the lane runtime (`process` or `acp`) when
    /// `--runtime` is omitted (issue #331). Validated at use, like `deps`.
    pub fn default_runtime(&self) -> Option<&str> {
        self.defaults.runtime.as_deref()
    }

    /// `defaults.prepare`: shell commands run in every new workspace after
    /// dependency prep, for generated gitignored files (issue #301).
    pub fn default_prepare(&self) -> &[String] {
        &self.defaults.prepare
    }

    /// The model for one lane of `agent` (issue #387): `--model`, then
    /// `--tier` looked up in `[tiers]`, then `defaults.model`, then `None`
    /// for the CLI's own default. A tier that is unknown or has no entry
    /// for `agent` is an error, so a typo never falls back to the CLI's
    /// default and its price.
    pub fn resolve_model(&self, model: Option<&str>, tier: Option<&str>, agent: &str) -> Result<Option<String>> {
        if let Some(model) = model {
            return Ok(Some(model.to_string()));
        }
        if let Some(tier) = tier {
            let Some(models) = self.tiers.get(tier) else {
                let known: Vec<&str> = self.tiers.keys().map(String::as_str).collect();
                bail!(
                    "--tier '{tier}' is not defined in pact.toml (defined: {})",
                    if known.is_empty() { "none; add a [tiers.<name>] table".to_string() } else { known.join(", ") }
                );
            };
            return match models.get(agent) {
                Some(model) => Ok(Some(model.clone())),
                None => bail!("tier '{tier}' in pact.toml has no model for agent '{agent}'; add `{agent} = \"<model>\"` under [tiers.{tier}]"),
            };
        }
        Ok(self.defaults.model.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pact-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_returns_default_when_no_file_present() {
        let dir = scratch_dir();
        let config = PactConfig::load(&dir).unwrap();
        assert_eq!(config.default_agent(), None);
        assert_eq!(config.default_safety(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_defaults_section() {
        let dir = scratch_dir();
        std::fs::write(
            dir.join(PactConfig::FILE_NAME),
            "[defaults]\nagent = \"copilot\"\nsafety = \"acceptEdits\"\ndeps = \"link\"\nmax_concurrent = 3\nmin_free_mem_mb = 0\nstagger_ms = 500\nper_lane_reserve_mb = 900\n",
        )
        .unwrap();
        let config = PactConfig::load(&dir).unwrap();
        assert_eq!(config.default_agent(), Some("copilot"));
        assert_eq!(config.default_safety(), Some("acceptEdits"));
        assert_eq!(config.default_deps(), Some("link"));
        assert_eq!(config.default_max_concurrent(), Some(3));
        assert_eq!(config.default_min_free_mem_mb(), Some(0));
        assert_eq!(config.default_stagger_ms(), Some(500));
        assert_eq!(config.default_per_lane_reserve_mb(), Some(900));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_model_prefers_flag_then_tier_then_default() {
        let config: PactConfig = toml::from_str(
            "[defaults]
model = \"sonnet\"
[tiers.chore]
claude = \"haiku\"
codex = \"gpt-5-mini\"
",
        )
        .unwrap();
        assert_eq!(config.resolve_model(Some("opus"), Some("chore"), "claude").unwrap().as_deref(), Some("opus"));
        assert_eq!(config.resolve_model(None, Some("chore"), "codex").unwrap().as_deref(), Some("gpt-5-mini"));
        assert_eq!(config.resolve_model(None, None, "claude").unwrap().as_deref(), Some("sonnet"));
        assert!(config.resolve_model(None, Some("chore"), "copilot").is_err());
        assert!(config.resolve_model(None, Some("plan"), "claude").is_err());
        assert_eq!(PactConfig::default().resolve_model(None, None, "claude").unwrap(), None);
    }

    #[test]
    fn load_tolerates_a_missing_defaults_section() {
        let dir = scratch_dir();
        std::fs::write(dir.join(PactConfig::FILE_NAME), "").unwrap();
        let config = PactConfig::load(&dir).unwrap();
        assert_eq!(config.default_agent(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_errors_on_malformed_toml() {
        let dir = scratch_dir();
        std::fs::write(dir.join(PactConfig::FILE_NAME), "this is not [ valid toml").unwrap();
        assert!(PactConfig::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
