//! Persistent TUI configuration (`~/.config/snowcone/config.toml`).
//!
//! Discovery stays zero-config; the only thing worth remembering across
//! runs is which managers the user has switched off in the TUI. The file
//! is written the first time a manager is toggled and never touched
//! otherwise, so a broken or missing file can always fall back to
//! defaults without losing anything.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub managers: ManagersConfig,
    #[serde(skip)]
    path: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ManagersConfig {
    /// Backend ids excluded from TUI searches, listings, and elections.
    /// May name backends that aren't detected (or even implemented) right
    /// now - those entries are preserved so the preference survives the
    /// tool disappearing and coming back.
    #[serde(default)]
    pub disabled: Vec<String>,
}

impl Config {
    /// Load from the default path. Never fails: a missing file (or a host
    /// with no resolvable config dir) is the default config, and an
    /// unreadable or unparsable file falls back to defaults plus a warning
    /// for the status line.
    pub fn load() -> (Self, Option<String>) {
        match config_path() {
            Some(path) => Self::load_from(path),
            None => (Self::default(), None),
        }
    }

    fn load_from(path: PathBuf) -> (Self, Option<String>) {
        let fallback = |path: PathBuf, warning: String| {
            let config = Self {
                path: Some(path),
                ..Self::default()
            };
            (config, Some(warning))
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Self {
                    path: Some(path),
                    ..Self::default()
                };
                return (config, None);
            }
            Err(error) => {
                let warning = format!("config: {}: {error}; using defaults", path.display());
                return fallback(path, warning);
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(mut config) => {
                config.path = Some(path);
                (config, None)
            }
            Err(error) => {
                let detail = error.to_string().replace('\n', " ");
                let warning = format!("config: {}: {detail}; using defaults", path.display());
                fallback(path, warning)
            }
        }
    }

    /// Write atomically (tmp + rename) so a crash never truncates the file.
    /// Synchronous on purpose: writes are tiny and happen on a keypress.
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            anyhow::bail!(
                "no config path on this host ($XDG_CONFIG_HOME, $HOME, and %APPDATA% unset)"
            );
        };
        let parent = path.parent().expect("config path always has a parent");
        std::fs::create_dir_all(parent)?;
        let rendered = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, rendered)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn disabled_set(&self) -> BTreeSet<String> {
        self.managers.disabled.iter().cloned().collect()
    }

    pub fn is_disabled(&self, id: &str) -> bool {
        self.managers.disabled.iter().any(|disabled| disabled == id)
    }

    pub fn set_disabled(&mut self, id: &str, disabled: bool) {
        if disabled {
            if !self.is_disabled(id) {
                self.managers.disabled.push(id.to_string());
                self.managers.disabled.sort();
            }
        } else {
            self.managers.disabled.retain(|entry| entry != id);
        }
    }
}

fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("snowcone").join("config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("snowcone")
                .join("config.toml"),
        );
    }
    // Windows has neither variable; %APPDATA% is the per-user config root.
    std::env::var_os("APPDATA")
        .filter(|appdata| !appdata.is_empty())
        .map(|appdata| PathBuf::from(appdata).join("snowcone").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snowcone-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn missing_file_is_default_without_warning() {
        let (config, warning) = Config::load_from(scratch("missing.toml"));
        assert!(warning.is_none());
        assert!(config.managers.disabled.is_empty());
    }

    #[test]
    fn save_and_reload_round_trip() {
        let path = scratch("roundtrip.toml");
        let (mut config, _) = Config::load_from(path.clone());
        config.set_disabled("pip", true);
        config.set_disabled("aptitude", true);
        config.set_disabled("pip", true); // idempotent
        config.save().unwrap();
        let (reloaded, warning) = Config::load_from(path);
        assert!(warning.is_none());
        assert_eq!(reloaded.managers.disabled, vec!["aptitude", "pip"]);
        assert!(reloaded.is_disabled("pip"));
        assert!(!reloaded.is_disabled("apt"));
    }

    #[test]
    fn parse_error_falls_back_with_warning() {
        let path = scratch("broken.toml");
        std::fs::write(&path, "not [valid toml").unwrap();
        let (config, warning) = Config::load_from(path);
        assert!(warning.is_some());
        assert!(config.managers.disabled.is_empty());
    }

    #[test]
    fn unknown_ids_survive_a_toggle_and_save() {
        let path = scratch("unknown.toml");
        std::fs::write(&path, "[managers]\ndisabled = [\"not-a-backend\"]\n").unwrap();
        let (mut config, _) = Config::load_from(path.clone());
        config.set_disabled("pip", true);
        config.set_disabled("pip", false);
        config.save().unwrap();
        let (reloaded, _) = Config::load_from(path);
        assert_eq!(reloaded.managers.disabled, vec!["not-a-backend"]);
    }
}
