//! Configuration loading and generation.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Main configuration structure.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Enable debug logging to file
    pub debug: bool,

    /// Path to log directory
    pub log_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            debug: false,
            log_path: default_log_path(),
        }
    }
}

/// Default log path: ~/.config/astro-sight/logs
fn default_log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("astro-sight")
        .join("logs")
}

/// Configuration service.
pub struct ConfigService;

impl ConfigService {
    /// Get the default configuration file path.
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("astro-sight")
            .join("config.toml")
    }

    /// Load configuration from file.
    ///
    /// If `path` is `None`, uses the default path.
    /// If the file doesn't exist, returns default configuration.
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        let config_dir = path.parent();

        if !path.exists() {
            // Return defaults — don't auto-create
            let mut config = Config::default();
            if let Some(dir) = config_dir {
                config.log_path = dir.join("logs");
            }
            return Ok(config);
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        // If log_path was not explicitly set, use config file's directory/logs
        if config.log_path == default_log_path()
            && let Some(dir) = config_dir
        {
            config.log_path = dir.join("logs");
        }

        Ok(config)
    }

    /// Generate default configuration file at the default path.
    pub fn generate_default() -> Result<()> {
        Self::generate_at(&Self::default_path())
    }

    /// Generate default configuration file at the specified path.
    pub fn generate_at(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory: {}", parent.display())
            })?;
        }

        let content = Self::default_config_content();
        fs::write(path, content)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;

        Ok(())
    }

    /// Generate default configuration content with comments.
    fn default_config_content() -> String {
        r#"# astro-sight configuration file
# https://github.com/owayo/astro-sight

# Enable debug logging to file (default: false)
debug = false

# Path to log directory (default: ~/.config/astro-sight/logs)
# log_path = "~/.config/astro-sight/logs"
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_path_ends_with_config_toml() {
        let path = ConfigService::default_path();
        assert!(path.ends_with("astro-sight/config.toml"));
    }

    #[test]
    fn test_default_path_contains_dot_config() {
        let path = ConfigService::default_path();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains(".config"),
            "Path should contain .config: {path_str}",
        );
    }

    #[test]
    fn test_generate_at_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("test_config.toml");

        ConfigService::generate_at(&config_path).unwrap();

        assert!(config_path.exists());
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("debug = false"));
        assert!(content.contains("log_path"));
    }

    #[test]
    fn test_generate_at_creates_parent_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nested").join("dir").join("config.toml");

        ConfigService::generate_at(&config_path).unwrap();

        assert!(config_path.exists());
    }

    #[test]
    fn test_load_returns_defaults_when_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nonexistent.toml");

        let config = ConfigService::load(Some(&config_path)).unwrap();

        // Should return defaults without creating file
        assert!(!config_path.exists());
        assert!(!config.debug);
    }

    #[test]
    fn test_load_parses_existing_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        fs::write(&config_path, "debug = true\n").unwrap();

        let config = ConfigService::load(Some(&config_path)).unwrap();
        assert!(config.debug);
    }

    #[test]
    fn test_load_invalid_toml_returns_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("bad.toml");

        fs::write(&config_path, "not valid [[[").unwrap();

        let result = ConfigService::load(Some(&config_path));
        assert!(result.is_err());
    }

    #[test]
    fn test_default_config_content_has_all_fields() {
        let content = ConfigService::default_config_content();
        assert!(content.contains("debug = false"));
        assert!(content.contains("log_path"));
    }

    #[test]
    fn test_load_custom_log_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            "debug = true\nlog_path = \"/tmp/astro-logs\"\n",
        )
        .unwrap();

        let config = ConfigService::load(Some(&config_path)).unwrap();
        assert!(config.debug);
        assert_eq!(config.log_path, PathBuf::from("/tmp/astro-logs"));
    }
}
