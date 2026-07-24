use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Repo-local defaults for `--agent`/`--safety`, read from `pact.toml` at
/// the repo root -- see DESIGN.md ("pact-cli > `pact.toml` / `pact init`").
/// A CLI flag always wins when both are given; this only fills in what the
/// flag would otherwise have defaulted to.
#[derive(Debug, Default, Deserialize)]
pub struct PactConfig {
    #[serde(default)]
    defaults: Defaults,
}

#[derive(Debug, Default, Deserialize)]
struct Defaults {
    agent: Option<String>,
    safety: Option<String>,
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
            "[defaults]\nagent = \"copilot\"\nsafety = \"acceptEdits\"\n",
        )
        .unwrap();
        let config = PactConfig::load(&dir).unwrap();
        assert_eq!(config.default_agent(), Some("copilot"));
        assert_eq!(config.default_safety(), Some("acceptEdits"));
        let _ = std::fs::remove_dir_all(&dir);
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
