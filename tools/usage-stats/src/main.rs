use chrono::{Datelike, Duration, Local, NaiveDate};
use comfy_table::{Attribute, Cell, CellAlignment, Table, presets::UTF8_FULL_CONDENSED};
use glob::glob;
use rayon::prelude::*;
use simd_json::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ── Types ──

#[derive(serde::Serialize, Clone)]
struct ToolDetail {
    tool: String,
    detail: String,
}

#[derive(serde::Serialize, Clone)]
struct Interaction {
    source: String,
    project: String,
    date: String,
    user_prompt: String,
    tools: Vec<ToolDetail>,
}

#[derive(Default)]
struct Stats {
    tool_counts: HashMap<String, HashMap<String, u64>>,
    project_tool_counts: HashMap<String, HashMap<String, HashMap<String, u64>>>,
    astro_subcmds: HashMap<String, HashMap<String, u64>>,
    astro_daily: HashMap<String, u64>,
    bash_cmd_counts: HashMap<String, HashMap<String, u64>>,
    session_counts: HashMap<String, u64>,
    file_counts: HashMap<String, u64>,
    total_tool_calls: HashMap<String, u64>,
    interactions: Vec<Interaction>,
}

impl Stats {
    fn merge(&mut self, other: Stats) {
        for (src, tools) in other.tool_counts {
            let entry = self.tool_counts.entry(src).or_default();
            for (tool, count) in tools {
                *entry.entry(tool).or_default() += count;
            }
        }
        for (src, projects) in other.project_tool_counts {
            let src_entry = self.project_tool_counts.entry(src).or_default();
            for (project, tools) in projects {
                let proj_entry = src_entry.entry(project).or_default();
                for (tool, count) in tools {
                    *proj_entry.entry(tool).or_default() += count;
                }
            }
        }
        for (src, subcmds) in other.astro_subcmds {
            let entry = self.astro_subcmds.entry(src).or_default();
            for (subcmd, count) in subcmds {
                *entry.entry(subcmd).or_default() += count;
            }
        }
        for (date, count) in other.astro_daily {
            *self.astro_daily.entry(date).or_default() += count;
        }
        for (src, count) in other.session_counts {
            *self.session_counts.entry(src).or_default() += count;
        }
        for (src, count) in other.file_counts {
            *self.file_counts.entry(src).or_default() += count;
        }
        for (src, cmds) in other.bash_cmd_counts {
            let entry = self.bash_cmd_counts.entry(src).or_default();
            for (cmd, count) in cmds {
                *entry.entry(cmd).or_default() += count;
            }
        }
        for (src, count) in other.total_tool_calls {
            *self.total_tool_calls.entry(src).or_default() += count;
        }
        self.interactions.extend(other.interactions);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

// ── Parsing ──

const KNOWN_SUBCMDS: &[&str] = &[
    "ast",
    "symbols",
    "calls",
    "refs",
    "context",
    "imports",
    "lint",
    "sequence",
    "cochange",
    "doctor",
    "session",
    "mcp",
    "init",
    "skill-install",
];

fn extract_astro_subcmd(cmd: &str) -> Option<&str> {
    let idx = cmd.find("astro-sight")?;
    let after = &cmd[idx + "astro-sight".len()..];
    let after = skip_flags(after.trim_start());
    let subcmd = after.split_whitespace().next()?;
    KNOWN_SUBCMDS
        .iter()
        .find(|&&known| known == subcmd)
        .copied()
}

fn skip_flags(s: &str) -> &str {
    let mut s = s;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            s = &rest[end..];
        } else if s.starts_with('-') && s.len() > 1 && s.as_bytes()[1].is_ascii_alphabetic() {
            let end = s[1..].find(char::is_whitespace).unwrap_or(s.len() - 1);
            s = &s[1 + end..];
        } else {
            break;
        }
    }
    s
}

fn is_astro_sight_cmd(cmd: &str) -> bool {
    cmd.contains("astro-sight") && !cmd.contains("cargo")
}

fn extract_bash_category(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }

    // Skip env var prefixes (e.g., RUST_LOG=debug cargo build)
    let mut s = trimmed;
    loop {
        let token = match s.split_whitespace().next() {
            Some(t) => t,
            None => return "(empty)".to_string(),
        };
        if token.contains('=') && !token.starts_with('=') {
            s = s[s.find(token).unwrap() + token.len()..].trim_start();
        } else {
            break;
        }
    }

    // Take first command (before && || ; |)
    let first_cmd = s
        .split("&&")
        .next()
        .unwrap_or(s)
        .split("||")
        .next()
        .unwrap_or(s)
        .split(';')
        .next()
        .unwrap_or(s)
        .trim();

    // Extract the command name (strip path prefix)
    let cmd_name = match first_cmd.split_whitespace().next() {
        Some(name) => name.rsplit('/').next().unwrap_or(name),
        None => return "(empty)".to_string(),
    };

    cmd_name.to_string()
}

fn extract_user_text(val: &simd_json::BorrowedValue) -> String {
    // message.content can be a string or array of text blocks
    let content = match val.get("message").and_then(|m| m.get("content")) {
        Some(c) => c,
        None => return String::new(),
    };
    if let Some(s) = content.as_str() {
        return truncate(s.trim(), 200);
    }
    if let Some(arr) = content.as_array() {
        let mut parts = Vec::new();
        for block in arr {
            if block.get("type").and_then(|v| v.as_str()) == Some("text")
                && let Some(t) = block.get("text").and_then(|v| v.as_str())
            {
                parts.push(t);
            }
        }
        return truncate(parts.join(" ").trim(), 200);
    }
    String::new()
}

fn process_claude_file(path: &Path, project: &str) -> Stats {
    let mut stats = Stats::default();
    let source = "claude-code".to_string();

    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => return stats,
    };

    *stats.file_counts.entry(source.clone()).or_default() += 1;

    let mut current_date = String::new();
    let mut session_counted = false;
    let mut current_user_prompt = String::new();
    let mut pending_tools: Vec<ToolDetail> = Vec::new();
    let mut has_relevant_tool = false;

    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut line_buf = line.to_vec();
        let val = match simd_json::to_borrowed_value(&mut line_buf) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = match val.get("type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        if current_date.is_empty()
            && let Some(ts) = val.get("timestamp").and_then(|v| v.as_str())
            && ts.len() >= 10
        {
            current_date = ts[..10].to_string();
        }

        if msg_type == "user" {
            // Flush previous interaction if it had relevant tools
            if has_relevant_tool && !current_user_prompt.is_empty() {
                stats.interactions.push(Interaction {
                    source: source.clone(),
                    project: project.to_string(),
                    date: current_date.clone(),
                    user_prompt: current_user_prompt.clone(),
                    tools: std::mem::take(&mut pending_tools),
                });
            } else {
                pending_tools.clear();
            }
            has_relevant_tool = false;
            current_user_prompt = extract_user_text(&val);

            if !session_counted {
                *stats.session_counts.entry(source.clone()).or_default() += 1;
                session_counted = true;
            }
            continue;
        }

        if msg_type != "assistant" {
            continue;
        }

        let content = match val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            Some(arr) => arr,
            None => continue,
        };

        for block in content {
            let block_type = match block.get("type").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => continue,
            };
            if block_type != "tool_use" {
                continue;
            }

            let tool_name = match block.get("name").and_then(|v| v.as_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            *stats
                .tool_counts
                .entry(source.clone())
                .or_default()
                .entry(tool_name.clone())
                .or_default() += 1;

            *stats
                .project_tool_counts
                .entry(source.clone())
                .or_default()
                .entry(project.to_string())
                .or_default()
                .entry(tool_name.clone())
                .or_default() += 1;

            *stats.total_tool_calls.entry(source.clone()).or_default() += 1;

            // Track Grep details
            if tool_name == "Grep" {
                let pattern = block
                    .get("input")
                    .and_then(|i| i.get("pattern"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let search_path = block
                    .get("input")
                    .and_then(|i| i.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or(".");
                pending_tools.push(ToolDetail {
                    tool: "Grep".to_string(),
                    detail: format!("pattern={pattern} path={search_path}"),
                });
                has_relevant_tool = true;
            }

            if tool_name == "Bash" {
                let cmd = block
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");

                let category = if is_astro_sight_cmd(cmd) {
                    "astro-sight".to_string()
                } else {
                    extract_bash_category(cmd)
                };
                *stats
                    .bash_cmd_counts
                    .entry(source.clone())
                    .or_default()
                    .entry(category)
                    .or_default() += 1;

                if is_astro_sight_cmd(cmd) {
                    let subcmd = extract_astro_subcmd(cmd).unwrap_or("unknown");
                    pending_tools.push(ToolDetail {
                        tool: "astro-sight".to_string(),
                        detail: truncate(cmd, 200),
                    });
                    has_relevant_tool = true;
                    *stats
                        .astro_subcmds
                        .entry(source.clone())
                        .or_default()
                        .entry(subcmd.to_string())
                        .or_default() += 1;
                    if !current_date.is_empty() {
                        *stats.astro_daily.entry(current_date.clone()).or_default() += 1;
                    }
                }
            }

            if tool_name.starts_with("mcp__astro") || tool_name.contains("astro_sight") {
                pending_tools.push(ToolDetail {
                    tool: tool_name.clone(),
                    detail: String::new(),
                });
                has_relevant_tool = true;
                *stats
                    .astro_subcmds
                    .entry(source.clone())
                    .or_default()
                    .entry(tool_name.clone())
                    .or_default() += 1;
                if !current_date.is_empty() {
                    *stats.astro_daily.entry(current_date.clone()).or_default() += 1;
                }
            }
        }
    }

    // Flush last interaction
    if has_relevant_tool && !current_user_prompt.is_empty() {
        stats.interactions.push(Interaction {
            source: source.clone(),
            project: project.to_string(),
            date: current_date.clone(),
            user_prompt: current_user_prompt,
            tools: pending_tools,
        });
    }

    stats
}

fn process_codex_file(path: &Path, exclude_project: Option<&str>) -> Stats {
    let mut stats = Stats::default();
    let source = "codex".to_string();

    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => return stats,
    };

    *stats.file_counts.entry(source.clone()).or_default() += 1;
    *stats.session_counts.entry(source.clone()).or_default() += 1;

    let file_date = extract_date_from_path(path);
    let mut project = String::new();
    let mut current_user_prompt = String::new();
    let mut pending_tools: Vec<ToolDetail> = Vec::new();
    let mut has_relevant_tool = false;

    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut line_buf = line.to_vec();
        let val = match simd_json::to_borrowed_value(&mut line_buf) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = match val.get("type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        if msg_type == "session_meta"
            && project.is_empty()
            && let Some(cwd) = val
                .get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
        {
            project = cwd.rsplit('/').next().unwrap_or("unknown").to_string();
            if let Some(excluded) = exclude_project
                && project == excluded
            {
                return Stats::default();
            }
        }

        if msg_type != "response_item" {
            continue;
        }

        let payload = match val.get("payload") {
            Some(p) => p,
            None => continue,
        };

        let item_type = match payload.get("type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        // Capture user prompt from response_item with role=user
        if item_type == "message" && payload.get("role").and_then(|r| r.as_str()) == Some("user") {
            // Flush previous
            if has_relevant_tool && !current_user_prompt.is_empty() {
                let proj = if project.is_empty() {
                    "unknown"
                } else {
                    &project
                };
                stats.interactions.push(Interaction {
                    source: source.clone(),
                    project: proj.to_string(),
                    date: file_date.clone(),
                    user_prompt: current_user_prompt.clone(),
                    tools: std::mem::take(&mut pending_tools),
                });
            } else {
                pending_tools.clear();
            }
            has_relevant_tool = false;

            // Extract text from content array
            let mut text_parts = Vec::new();
            if let Some(arr) = payload.get("content").and_then(|c| c.as_array()) {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("input_text")
                        && let Some(t) = block.get("text").and_then(|t| t.as_str())
                    {
                        text_parts.push(t);
                    }
                }
            }
            let joined = text_parts.join(" ");
            // Skip system/instructions content, only keep actual user input
            if !joined.contains("AGENTS.md instructions") && !joined.is_empty() {
                current_user_prompt = truncate(joined.trim(), 200);
            }
            continue;
        }

        if item_type != "function_call" {
            continue;
        }

        let func_name = match payload.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        *stats
            .tool_counts
            .entry(source.clone())
            .or_default()
            .entry(func_name.clone())
            .or_default() += 1;

        let proj = if project.is_empty() {
            "unknown"
        } else {
            &project
        };
        *stats
            .project_tool_counts
            .entry(source.clone())
            .or_default()
            .entry(proj.to_string())
            .or_default()
            .entry(func_name.clone())
            .or_default() += 1;

        *stats.total_tool_calls.entry(source.clone()).or_default() += 1;

        let args = payload
            .get("arguments")
            .and_then(|a| a.as_str())
            .unwrap_or("");

        // Track grep/rg and bash category in codex exec_command
        if func_name == "exec_command" {
            let mut args_buf = args.as_bytes().to_vec();
            if let Ok(args_val) = simd_json::to_borrowed_value(&mut args_buf) {
                let cmd = args_val.get("cmd").and_then(|c| c.as_str()).unwrap_or("");

                let category = if is_astro_sight_cmd(cmd) {
                    "astro-sight".to_string()
                } else {
                    extract_bash_category(cmd)
                };
                *stats
                    .bash_cmd_counts
                    .entry(source.clone())
                    .or_default()
                    .entry(category)
                    .or_default() += 1;

                if cmd.starts_with("grep ")
                    || cmd.starts_with("rg ")
                    || cmd.contains("| grep")
                    || cmd.contains("| rg")
                {
                    pending_tools.push(ToolDetail {
                        tool: "grep/rg".to_string(),
                        detail: truncate(cmd, 200),
                    });
                    has_relevant_tool = true;
                }
            }
        }

        if args.contains("astro-sight") && !args.contains("cargo") {
            let mut args_buf = args.as_bytes().to_vec();
            if let Ok(args_val) = simd_json::to_borrowed_value(&mut args_buf) {
                let cmd = args_val.get("cmd").and_then(|c| c.as_str()).unwrap_or(args);
                pending_tools.push(ToolDetail {
                    tool: "astro-sight".to_string(),
                    detail: truncate(cmd, 200),
                });
                has_relevant_tool = true;
                if let Some(subcmd) = extract_astro_subcmd(cmd) {
                    *stats
                        .astro_subcmds
                        .entry(source.clone())
                        .or_default()
                        .entry(subcmd.to_string())
                        .or_default() += 1;
                }
            }
            if !file_date.is_empty() {
                *stats.astro_daily.entry(file_date.clone()).or_default() += 1;
            }
        }
    }

    // Flush last
    if has_relevant_tool && !current_user_prompt.is_empty() {
        let proj = if project.is_empty() {
            "unknown"
        } else {
            &project
        };
        stats.interactions.push(Interaction {
            source: source.clone(),
            project: proj.to_string(),
            date: file_date,
            user_prompt: current_user_prompt,
            tools: pending_tools,
        });
    }

    stats
}

fn extract_date_from_path(path: &Path) -> String {
    let s = match path.to_str() {
        Some(s) => s,
        None => return String::new(),
    };
    let parts: Vec<&str> = s.split('/').collect();
    for window in parts.windows(3) {
        if let (Ok(y), Ok(m), Ok(d)) = (
            window[0].parse::<i32>(),
            window[1].parse::<u32>(),
            window[2].parse::<u32>(),
        ) && (2020..=2030).contains(&y)
            && (1..=12).contains(&m)
            && (1..=31).contains(&d)
        {
            return format!("{y:04}-{m:02}-{d:02}");
        }
    }
    String::new()
}

// ── File Discovery ──

/// Cutoff date (inclusive). None means no filtering.
fn cutoff_date(days: Option<u32>) -> Option<NaiveDate> {
    days.map(|d| (Local::now() - Duration::days(i64::from(d) - 1)).date_naive())
}

fn is_modified_since(path: &Path, since: NaiveDate) -> bool {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            let dt: chrono::DateTime<Local> = t.into();
            dt.date_naive() >= since
        })
        .unwrap_or(false)
}

fn find_claude_files(
    cutoff: Option<NaiveDate>,
    exclude_project: Option<&str>,
) -> Vec<(PathBuf, String)> {
    let home = dirs::home_dir().expect("cannot find home dir");
    let base = home.join(".claude/projects");
    let pattern = format!("{}/*/*.jsonl", base.display());

    let mut files = Vec::new();
    for path in glob(&pattern).expect("invalid glob").flatten() {
        if let Some(since) = cutoff
            && !is_modified_since(&path, since)
        {
            continue;
        }
        if let Some(parent) = path.parent() {
            let project = parent
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            if let Some(excluded) = exclude_project
                && shorten_project_name(&project) == excluded
            {
                continue;
            }
            files.push((path, project));
        }
    }
    files
}

fn find_codex_files(cutoff: Option<NaiveDate>) -> Vec<PathBuf> {
    let home = dirs::home_dir().expect("cannot find home dir");
    let base = home.join(".codex/sessions");

    if let Some(since) = cutoff {
        // Collect from each day directory in range
        let today = Local::now().date_naive();
        let mut files = Vec::new();
        let mut date = since;
        while date <= today {
            let day_dir = base.join(format!(
                "{}/{:02}/{:02}",
                date.year(),
                date.month(),
                date.day()
            ));
            let pattern = format!("{}/*.jsonl", day_dir.display());
            files.extend(glob(&pattern).expect("invalid glob").flatten());
            date += Duration::days(1);
        }
        files
    } else {
        let pattern = format!("{}/**/*.jsonl", base.display());
        glob(&pattern).expect("invalid glob").flatten().collect()
    }
}

// ── Display ──

fn shorten_project_name(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    if let Some(idx) = parts.iter().position(|&p| p == "GitHub") {
        return parts[idx + 1..].join("-");
    }
    name.to_string()
}

fn print_summary(stats: &Stats, label: &str) {
    println!("\n╔══════════════════════════════════════════════╗");
    println!("║  astro-sight Usage Statistics  {:<15}║", label);
    println!("╚══════════════════════════════════════════════╝\n");

    // Overview
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec![
        Cell::new("Source").add_attribute(Attribute::Bold),
        Cell::new("Files").add_attribute(Attribute::Bold),
        Cell::new("Sessions").add_attribute(Attribute::Bold),
        Cell::new("Tool Calls").add_attribute(Attribute::Bold),
        Cell::new("astro-sight").add_attribute(Attribute::Bold),
    ]);

    for src in &["claude-code", "codex"] {
        let files = stats.file_counts.get(*src).copied().unwrap_or(0);
        let sessions = stats.session_counts.get(*src).copied().unwrap_or(0);
        let total = stats.total_tool_calls.get(*src).copied().unwrap_or(0);
        let astro: u64 = stats
            .astro_subcmds
            .get(*src)
            .map(|m| m.values().sum())
            .unwrap_or(0);
        table.add_row(vec![
            Cell::new(src),
            Cell::new(files).set_alignment(CellAlignment::Right),
            Cell::new(sessions).set_alignment(CellAlignment::Right),
            Cell::new(total).set_alignment(CellAlignment::Right),
            Cell::new(astro).set_alignment(CellAlignment::Right),
        ]);
    }
    println!("## Overview\n{table}\n");

    // Tool distribution (top 20 per source)
    for src in &["claude-code", "codex"] {
        if let Some(tools) = stats.tool_counts.get(*src) {
            let mut sorted: Vec<_> = tools.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));

            let mut table = Table::new();
            table.load_preset(UTF8_FULL_CONDENSED);
            table.set_header(vec![
                Cell::new("Tool").add_attribute(Attribute::Bold),
                Cell::new("Count").add_attribute(Attribute::Bold),
                Cell::new("%").add_attribute(Attribute::Bold),
            ]);

            let total: u64 = sorted.iter().map(|(_, c)| **c).sum();
            for (name, count) in sorted.iter().take(20) {
                let pct = if total > 0 {
                    (**count as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                table.add_row(vec![
                    Cell::new(name),
                    Cell::new(count).set_alignment(CellAlignment::Right),
                    Cell::new(format!("{pct:.1}")).set_alignment(CellAlignment::Right),
                ]);
            }
            println!("## Tool Distribution [{src}] (top 20)\n{table}\n");

            // Bash breakdown
            if let Some(bash_cmds) = stats.bash_cmd_counts.get(*src) {
                let mut sorted_cmds: Vec<_> = bash_cmds.iter().collect();
                sorted_cmds.sort_by(|a, b| b.1.cmp(a.1));

                let bash_total: u64 = sorted_cmds.iter().map(|(_, c)| **c).sum();
                let top_n = 15;
                let shown: Vec<_> = sorted_cmds.iter().take(top_n).copied().collect::<Vec<_>>();
                let shown_sum: u64 = shown.iter().map(|(_, c)| **c).sum();
                let other_count = bash_total - shown_sum;

                let mut bash_table = Table::new();
                bash_table.load_preset(UTF8_FULL_CONDENSED);
                bash_table.set_header(vec![
                    Cell::new("Command").add_attribute(Attribute::Bold),
                    Cell::new("Count").add_attribute(Attribute::Bold),
                    Cell::new("%").add_attribute(Attribute::Bold),
                ]);

                for (name, count) in &shown {
                    let pct = if bash_total > 0 {
                        (**count as f64 / bash_total as f64) * 100.0
                    } else {
                        0.0
                    };
                    bash_table.add_row(vec![
                        Cell::new(name),
                        Cell::new(count).set_alignment(CellAlignment::Right),
                        Cell::new(format!("{pct:.1}")).set_alignment(CellAlignment::Right),
                    ]);
                }

                if other_count > 0 {
                    let pct = (other_count as f64 / bash_total as f64) * 100.0;
                    bash_table.add_row(vec![
                        Cell::new("(other)"),
                        Cell::new(other_count).set_alignment(CellAlignment::Right),
                        Cell::new(format!("{pct:.1}")).set_alignment(CellAlignment::Right),
                    ]);
                }

                println!("## Bash Breakdown [{src}] (top {top_n})\n{bash_table}\n");
            }
        }
    }

    // astro-sight subcommand breakdown
    let has_astro = stats.astro_subcmds.values().any(|m| !m.is_empty());
    if has_astro {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec![
            Cell::new("Source").add_attribute(Attribute::Bold),
            Cell::new("Subcommand").add_attribute(Attribute::Bold),
            Cell::new("Count").add_attribute(Attribute::Bold),
        ]);

        for src in &["claude-code", "codex"] {
            if let Some(subcmds) = stats.astro_subcmds.get(*src) {
                let mut sorted: Vec<_> = subcmds.iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(a.1));
                for (subcmd, count) in &sorted {
                    table.add_row(vec![
                        Cell::new(src),
                        Cell::new(subcmd),
                        Cell::new(count).set_alignment(CellAlignment::Right),
                    ]);
                }
            }
        }
        println!("## astro-sight Subcommands\n{table}\n");
    }

    // Daily timeline (recent 30 days)
    if !stats.astro_daily.is_empty() {
        let mut dates: Vec<_> = stats.astro_daily.iter().collect();
        dates.sort_by(|a, b| a.0.cmp(b.0));

        let start = dates.len().saturating_sub(30);
        let recent = &dates[start..];

        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec![
            Cell::new("Date").add_attribute(Attribute::Bold),
            Cell::new("Calls").add_attribute(Attribute::Bold),
            Cell::new("").add_attribute(Attribute::Bold),
        ]);

        let max_count = recent.iter().map(|(_, c)| **c).max().unwrap_or(1);
        for (date, count) in recent {
            let bar_len = ((**count as f64 / max_count as f64) * 30.0) as usize;
            let bar = "\u{2588}".repeat(bar_len);
            table.add_row(vec![
                Cell::new(date),
                Cell::new(count).set_alignment(CellAlignment::Right),
                Cell::new(bar),
            ]);
        }
        println!("## astro-sight Daily Usage (recent 30 days)\n{table}\n");

        // Weekly summary
        let mut weekly: HashMap<String, u64> = HashMap::new();
        for (date_str, count) in &stats.astro_daily {
            if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                let iso_week = date.iso_week();
                let week_key = format!("{}-W{:02}", iso_week.year(), iso_week.week());
                *weekly.entry(week_key).or_default() += *count;
            }
        }
        if !weekly.is_empty() {
            let mut weeks: Vec<_> = weekly.iter().collect();
            weeks.sort_by(|a, b| a.0.cmp(b.0));
            let start = weeks.len().saturating_sub(12);
            let recent_weeks = &weeks[start..];

            let mut table = Table::new();
            table.load_preset(UTF8_FULL_CONDENSED);
            table.set_header(vec![
                Cell::new("Week").add_attribute(Attribute::Bold),
                Cell::new("Calls").add_attribute(Attribute::Bold),
                Cell::new("").add_attribute(Attribute::Bold),
            ]);

            let max_w = recent_weeks.iter().map(|(_, c)| **c).max().unwrap_or(1);
            for (week, count) in recent_weeks {
                let bar_len = ((**count as f64 / max_w as f64) * 30.0) as usize;
                let bar = "\u{2588}".repeat(bar_len);
                table.add_row(vec![
                    Cell::new(week),
                    Cell::new(count).set_alignment(CellAlignment::Right),
                    Cell::new(bar),
                ]);
            }
            println!("## astro-sight Weekly Usage (recent 12 weeks)\n{table}\n");
        }
    }

    // Top projects
    for src in &["claude-code", "codex"] {
        if let Some(projects) = stats.project_tool_counts.get(*src) {
            let mut proj_totals: Vec<_> = projects
                .iter()
                .map(|(proj, tools)| {
                    let total: u64 = tools.values().sum();
                    (proj, tools, total)
                })
                .collect();
            proj_totals.sort_by(|a, b| b.2.cmp(&a.2));

            let mut table = Table::new();
            table.load_preset(UTF8_FULL_CONDENSED);
            table.set_header(vec![
                Cell::new("Project").add_attribute(Attribute::Bold),
                Cell::new("Tool Calls").add_attribute(Attribute::Bold),
                Cell::new("Top Tools").add_attribute(Attribute::Bold),
            ]);

            for (proj, tools, total) in proj_totals.iter().take(15) {
                let mut sorted_tools: Vec<_> = tools.iter().collect();
                sorted_tools.sort_by(|a, b| b.1.cmp(a.1));
                let top3: Vec<String> = sorted_tools
                    .iter()
                    .take(3)
                    .map(|(name, count)| format!("{name}({count})"))
                    .collect();
                let short_proj = shorten_project_name(proj);
                table.add_row(vec![
                    Cell::new(short_proj),
                    Cell::new(total).set_alignment(CellAlignment::Right),
                    Cell::new(top3.join(", ")),
                ]);
            }
            println!("## Top Projects [{src}] (top 15)\n{table}\n");
        }
    }
}

/// Project name to exclude by default (the tool's own project).
const SELF_PROJECT: &str = "astro-sight";

struct CliArgs {
    days: Option<u32>,
    json: bool,
    include_self: bool,
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();

    let json = args.iter().any(|a| a == "--json" || a == "-j");
    let include_self = args.iter().any(|a| a == "--include-self");

    if args.iter().any(|a| a == "--all" || a == "-a") {
        return CliArgs {
            days: None,
            json,
            include_self,
        };
    }

    // --days N or -d N
    for (i, arg) in args.iter().enumerate() {
        if (arg == "--days" || arg == "-d")
            && i + 1 < args.len()
            && let Ok(n) = args[i + 1].parse::<u32>()
        {
            return CliArgs {
                days: Some(n),
                json,
                include_self,
            };
        }
        if let Some(val) = arg.strip_prefix("--days=")
            && let Ok(n) = val.parse::<u32>()
        {
            return CliArgs {
                days: Some(n),
                json,
                include_self,
            };
        }
    }

    CliArgs {
        days: Some(1),
        json,
        include_self,
    }
}

fn print_json(stats: &Stats, label: &str) {
    use serde::Serialize;

    #[derive(Serialize)]
    struct JsonOutput {
        period: String,
        sources: Vec<SourceSummary>,
        astro_sight: AstroSightSummary,
        interactions: Vec<Interaction>,
    }

    #[derive(Serialize)]
    struct SourceSummary {
        name: String,
        files_scanned: u64,
        sessions: u64,
        total_tool_calls: u64,
        astro_sight_calls: u64,
        adoption_rate_pct: f64,
        tool_distribution: Vec<ToolEntry>,
        bash_breakdown: Vec<ToolEntry>,
        top_projects: Vec<ProjectEntry>,
    }

    #[derive(Serialize)]
    struct ToolEntry {
        tool: String,
        count: u64,
        pct: f64,
    }

    #[derive(Serialize)]
    struct ProjectEntry {
        project: String,
        tool_calls: u64,
        top_tools: Vec<ToolEntry>,
    }

    #[derive(Serialize)]
    struct AstroSightSummary {
        subcommands: HashMap<String, Vec<SubcmdEntry>>,
        daily: Vec<DailyEntry>,
        weekly: Vec<WeeklyEntry>,
    }

    #[derive(Serialize)]
    struct SubcmdEntry {
        subcommand: String,
        count: u64,
    }

    #[derive(Serialize)]
    struct DailyEntry {
        date: String,
        calls: u64,
    }

    #[derive(Serialize)]
    struct WeeklyEntry {
        week: String,
        calls: u64,
    }

    let mut sources = Vec::new();
    for src in &["claude-code", "codex"] {
        let files = stats.file_counts.get(*src).copied().unwrap_or(0);
        let sessions = stats.session_counts.get(*src).copied().unwrap_or(0);
        let total = stats.total_tool_calls.get(*src).copied().unwrap_or(0);
        let astro: u64 = stats
            .astro_subcmds
            .get(*src)
            .map(|m| m.values().sum())
            .unwrap_or(0);
        let adoption = if total > 0 {
            (astro as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let mut tool_dist = Vec::new();
        if let Some(tools) = stats.tool_counts.get(*src) {
            let total_f = tools.values().sum::<u64>() as f64;
            let mut sorted: Vec<_> = tools.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));
            for (name, count) in sorted {
                let pct = if total_f > 0.0 {
                    (*count as f64 / total_f) * 100.0
                } else {
                    0.0
                };
                tool_dist.push(ToolEntry {
                    tool: name.clone(),
                    count: *count,
                    pct: (pct * 10.0).round() / 10.0,
                });
            }
        }

        let mut top_projects = Vec::new();
        if let Some(projects) = stats.project_tool_counts.get(*src) {
            let mut proj_totals: Vec<_> = projects
                .iter()
                .map(|(proj, tools)| {
                    let total: u64 = tools.values().sum();
                    (proj, tools, total)
                })
                .collect();
            proj_totals.sort_by(|a, b| b.2.cmp(&a.2));
            for (proj, tools, total) in proj_totals.iter().take(15) {
                let mut sorted_tools: Vec<_> = tools.iter().collect();
                sorted_tools.sort_by(|a, b| b.1.cmp(a.1));
                let top3: Vec<ToolEntry> = sorted_tools
                    .iter()
                    .take(3)
                    .map(|(name, count)| ToolEntry {
                        tool: name.to_string(),
                        count: **count,
                        pct: 0.0,
                    })
                    .collect();
                top_projects.push(ProjectEntry {
                    project: shorten_project_name(proj),
                    tool_calls: *total,
                    top_tools: top3,
                });
            }
        }

        let mut bash_breakdown = Vec::new();
        if let Some(bash_cmds) = stats.bash_cmd_counts.get(*src) {
            let bash_total = bash_cmds.values().sum::<u64>() as f64;
            let mut sorted: Vec<_> = bash_cmds.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));
            for (name, count) in sorted {
                let pct = if bash_total > 0.0 {
                    (*count as f64 / bash_total) * 100.0
                } else {
                    0.0
                };
                bash_breakdown.push(ToolEntry {
                    tool: name.clone(),
                    count: *count,
                    pct: (pct * 10.0).round() / 10.0,
                });
            }
        }

        sources.push(SourceSummary {
            name: src.to_string(),
            files_scanned: files,
            sessions,
            total_tool_calls: total,
            astro_sight_calls: astro,
            adoption_rate_pct: (adoption * 1000.0).round() / 1000.0,
            tool_distribution: tool_dist,
            bash_breakdown,
            top_projects,
        });
    }

    // Subcommands
    let mut subcommands: HashMap<String, Vec<SubcmdEntry>> = HashMap::new();
    for src in &["claude-code", "codex"] {
        if let Some(subcmds) = stats.astro_subcmds.get(*src) {
            let mut sorted: Vec<_> = subcmds.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));
            let entries: Vec<SubcmdEntry> = sorted
                .iter()
                .map(|(name, count)| SubcmdEntry {
                    subcommand: name.to_string(),
                    count: **count,
                })
                .collect();
            if !entries.is_empty() {
                subcommands.insert(src.to_string(), entries);
            }
        }
    }

    // Daily
    let mut daily: Vec<DailyEntry> = stats
        .astro_daily
        .iter()
        .map(|(date, count)| DailyEntry {
            date: date.clone(),
            calls: *count,
        })
        .collect();
    daily.sort_by(|a, b| a.date.cmp(&b.date));

    // Weekly
    let mut weekly_map: HashMap<String, u64> = HashMap::new();
    for (date_str, count) in &stats.astro_daily {
        if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            let iso_week = date.iso_week();
            let week_key = format!("{}-W{:02}", iso_week.year(), iso_week.week());
            *weekly_map.entry(week_key).or_default() += *count;
        }
    }
    let mut weekly: Vec<WeeklyEntry> = weekly_map
        .into_iter()
        .map(|(week, calls)| WeeklyEntry { week, calls })
        .collect();
    weekly.sort_by(|a, b| a.week.cmp(&b.week));

    let output = JsonOutput {
        period: label.to_string(),
        sources,
        astro_sight: AstroSightSummary {
            subcommands,
            daily,
            weekly,
        },
        interactions: stats.interactions.clone(),
    };

    println!(
        "{}",
        serde_json::to_string(&output).expect("JSON serialize failed")
    );
}

fn main() {
    let cli = parse_args();
    let cutoff = cutoff_date(cli.days);

    let exclude = if cli.include_self {
        None
    } else {
        Some(SELF_PROJECT)
    };

    let label = match cli.days {
        None => "[all time]".to_string(),
        Some(1) => format!("[{}]", Local::now().format("%Y-%m-%d")),
        Some(n) => format!("[last {n} days]"),
    };

    let claude_files = find_claude_files(cutoff, exclude);
    let codex_files = find_codex_files(cutoff);

    let hint = match cli.days {
        None => "(all time)".to_string(),
        Some(1) => "(today only, use --days N or --all)".to_string(),
        Some(n) => format!("(last {n} days, use --all for all time)"),
    };
    let exclude_hint = if exclude.is_some() {
        format!(", excluding '{SELF_PROJECT}' project")
    } else {
        String::new()
    };
    eprintln!(
        "Scanning {} Claude Code files, {} Codex files... {}{}",
        claude_files.len(),
        codex_files.len(),
        hint,
        exclude_hint,
    );

    let merged = Mutex::new(Stats::default());

    claude_files.par_iter().for_each(|(path, project)| {
        let stats = process_claude_file(path, project);
        merged.lock().unwrap().merge(stats);
    });

    codex_files.par_iter().for_each(|path| {
        let stats = process_codex_file(path, exclude);
        merged.lock().unwrap().merge(stats);
    });

    let stats = merged.into_inner().unwrap();

    if cli.json {
        print_json(&stats, &label);
    } else {
        print_summary(&stats, &label);
    }
}
