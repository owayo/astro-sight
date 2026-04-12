//! AI エージェント向けスキルのインストール処理。

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

const SKILL_CONTENT: &str = include_str!("../skills/SKILL.md");

/// 対応している AI エージェントのインストール先。
#[derive(Clone, Copy)]
enum Target {
    /// Claude Code: ~/.claude/skills/astro-sight/
    Claude,
    /// Codex CLI: ~/.codex/skills/astro-sight/
    Codex,
}

impl Target {
    fn parse(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => bail!("Unknown target: \"{other}\". Supported targets: claude, codex"),
        }
    }

    fn skill_dir_from_home(&self, home: &Path) -> PathBuf {
        let base = match self {
            Self::Claude => home.join(".claude").join("skills"),
            Self::Codex => home.join(".codex").join("skills"),
        };
        base.join("astro-sight")
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
        }
    }
}

/// 指定したターゲット向けに astro-sight スキルをインストールする。
pub fn install(target: &str) -> Result<()> {
    let target = Target::parse(target)?;
    let skill_path = install_to_dir(
        target,
        &dirs::home_dir().context("ホームディレクトリを判別できませんでした")?,
    )?;

    eprintln!(
        "Installed astro-sight skill for {} at: {}",
        target.name(),
        skill_path.display()
    );
    Ok(())
}

fn install_to_dir(target: Target, root: &Path) -> Result<PathBuf> {
    let skill_dir = target.skill_dir_from_home(root);

    fs::create_dir_all(&skill_dir).with_context(|| {
        format!(
            "ディレクトリを作成できませんでした: {}",
            skill_dir.display()
        )
    })?;

    let skill_path = skill_dir.join("SKILL.md");
    fs::write(&skill_path, SKILL_CONTENT)
        .with_context(|| format!("ファイルを書き込めませんでした: {}", skill_path.display()))?;

    Ok(skill_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn skill_dir_from_home_uses_target_specific_directory() {
        let home = Path::new("/tmp/astro-home");
        assert_eq!(
            Target::Claude.skill_dir_from_home(home),
            home.join(".claude").join("skills").join("astro-sight")
        );
        assert_eq!(
            Target::Codex.skill_dir_from_home(home),
            home.join(".codex").join("skills").join("astro-sight")
        );
    }

    #[test]
    fn install_to_dir_writes_skill_file_for_codex() {
        let temp = tempdir().unwrap();
        let skill_path = install_to_dir(Target::Codex, temp.path()).unwrap();

        assert_eq!(
            skill_path,
            temp.path()
                .join(".codex")
                .join("skills")
                .join("astro-sight")
                .join("SKILL.md")
        );
        assert_eq!(fs::read_to_string(skill_path).unwrap(), SKILL_CONTENT);
    }
}
