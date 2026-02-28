//! Skill installation for AI agents.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;

const SKILL_CONTENT: &str = include_str!("../skills/SKILL.md");

/// Supported AI agent targets.
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

    fn skill_dir(&self) -> Result<PathBuf> {
        let home = dirs::home_dir().context("Could not determine home directory")?;
        let base = match self {
            Self::Claude => home.join(".claude").join("skills"),
            Self::Codex => home.join(".codex").join("skills"),
        };
        Ok(base.join("astro-sight"))
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
        }
    }
}

/// Install the astro-sight skill for the specified target agent.
pub fn install(target: &str) -> Result<()> {
    let target = Target::parse(target)?;
    let skill_dir = target.skill_dir()?;

    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("Failed to create directory: {}", skill_dir.display()))?;

    let skill_path = skill_dir.join("SKILL.md");
    fs::write(&skill_path, SKILL_CONTENT)
        .with_context(|| format!("Failed to write: {}", skill_path.display()))?;

    eprintln!(
        "Installed astro-sight skill for {} at: {}",
        target.name(),
        skill_path.display()
    );
    Ok(())
}
