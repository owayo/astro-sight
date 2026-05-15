//! 設定ファイルの読み込みと生成。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// 実行時に使う設定。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// デバッグログをファイルに出力するかどうか。
    pub debug: bool,

    /// ログディレクトリのパス。
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

#[derive(Debug, Deserialize)]
struct RawConfig {
    debug: Option<bool>,
    log_path: Option<PathBuf>,
}

/// デフォルトのログ出力先: ~/.config/astro-sight/logs
fn default_log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("astro-sight")
        .join("logs")
}

/// 先頭の `~` または `~/` をユーザーのホームディレクトリへ展開する。
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else if let Some(rest) = s.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else {
        path.to_path_buf()
    }
}

/// 設定ファイルの読み書きを扱うサービス。
pub struct ConfigService;

impl ConfigService {
    /// デフォルトの設定ファイルパスを返す。
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("astro-sight")
            .join("config.toml")
    }

    /// 設定ファイルを読み込む。
    ///
    /// `path` が `None` の場合はデフォルトパスを使う。
    /// ファイルが存在しない場合はデフォルト設定を返す。
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        let config_dir = path.parent();

        if !path.exists() {
            // ファイルは自動生成せず、既定値だけを返す。
            let mut config = Config::default();
            if let Some(dir) = config_dir {
                config.log_path = dir.join("logs");
            }
            return Ok(config);
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let raw: RawConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        // log_path は明示指定と未指定を区別する。値の一致で判定すると、
        // ユーザーが既定パスを明示した場合に未指定扱いしてしまう。
        let log_path = match raw.log_path {
            Some(path) => expand_tilde(&path),
            None => config_dir
                .map(|dir| dir.join("logs"))
                .unwrap_or_else(default_log_path),
        };

        Ok(Config {
            debug: raw.debug.unwrap_or(false),
            log_path,
        })
    }

    /// デフォルトパスに設定ファイルを生成する。
    pub fn generate_default() -> Result<()> {
        Self::generate_at(&Self::default_path())
    }

    /// 指定パスに設定ファイルを生成する。
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

    /// コメント付きのデフォルト設定内容を生成する。
    fn default_config_content() -> String {
        r#"# astro-sight 設定ファイル
# https://github.com/owayo/astro-sight

# デバッグログをファイルに出力する (デフォルト: false)
debug = false

# ログディレクトリのパス (デフォルト: ~/.config/astro-sight/logs)
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

        // ファイルを作らず既定値だけを返す。
        assert!(!config_path.exists());
        assert!(!config.debug);
        assert_eq!(config.log_path, dir.path().join("logs"));
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

    #[test]
    fn test_load_missing_log_path_uses_config_dir_logs() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nested").join("config.toml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();

        fs::write(&config_path, "debug = true\n").unwrap();

        let config = ConfigService::load(Some(&config_path)).unwrap();
        assert_eq!(config.log_path, config_path.parent().unwrap().join("logs"));
    }

    #[test]
    fn test_load_explicit_default_log_path_is_respected_for_custom_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nested").join("config.toml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let explicit = default_log_path();

        fs::write(
            &config_path,
            format!("debug = true\nlog_path = \"{}\"\n", explicit.display()),
        )
        .unwrap();

        let config = ConfigService::load(Some(&config_path)).unwrap();
        assert_eq!(config.log_path, explicit);
    }

    #[test]
    fn test_expand_tilde_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde(Path::new("~")), home);
        assert_eq!(expand_tilde(Path::new("~/tmp")), home.join("tmp"));
        assert_eq!(
            expand_tilde(Path::new("~/a/b/c")),
            home.join("a").join("b").join("c"),
        );
    }

    #[test]
    fn test_expand_tilde_no_op() {
        assert_eq!(
            expand_tilde(Path::new("/tmp/logs")),
            PathBuf::from("/tmp/logs"),
        );
        assert_eq!(
            expand_tilde(Path::new("relative/path")),
            PathBuf::from("relative/path"),
        );
    }

    #[test]
    fn test_load_tilde_log_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        fs::write(&config_path, "debug = true\nlog_path = \"~/tmp\"\n").unwrap();

        let config = ConfigService::load(Some(&config_path)).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(config.log_path, home.join("tmp"));
    }
}
