use std::collections::HashMap;
use std::process::Command;

use anyhow::{Result, bail};

use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::{CoChangeEntry, CoChangeResult};

/// Analyze co-change patterns from git log.
///
/// - `dir`: the git repository directory
/// - `lookback`: number of recent commits to analyze
/// - `min_confidence`: minimum confidence threshold (0.0 to 1.0)
/// - `filter_file`: optional file path to filter results (only pairs containing this file)
pub fn analyze_cochange(
    dir: &str,
    lookback: usize,
    min_confidence: f64,
    filter_file: Option<&str>,
) -> Result<CoChangeResult> {
    // 1. Run git log to get commit file lists
    let output = Command::new("git")
        .args([
            "log",
            "--name-only",
            "--pretty=format:---COMMIT---",
            "-n",
            &lookback.to_string(),
        ])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(AstroError::new(
            ErrorCode::IoError,
            format!("git log failed: {stderr}"),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // 2. Parse commits
    let mut commits: Vec<Vec<String>> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "---COMMIT---" {
            if !current_files.is_empty() {
                commits.push(std::mem::take(&mut current_files));
            }
        } else if !trimmed.is_empty() {
            current_files.push(trimmed.to_string());
        }
    }
    // Don't forget the last commit
    if !current_files.is_empty() {
        commits.push(current_files);
    }

    let commits_analyzed = commits.len();

    // 3. Count individual file changes
    let mut file_counts: HashMap<String, usize> = HashMap::new();
    for commit in &commits {
        for file in commit {
            *file_counts.entry(file.clone()).or_insert(0) += 1;
        }
    }

    // 4. Count co-changes (file pairs that appear in the same commit)
    let mut pair_counts: HashMap<(String, String), usize> = HashMap::new();
    for commit in &commits {
        if commit.len() < 2 {
            continue;
        }
        // Generate all pairs (sorted to avoid duplicates)
        let mut files: Vec<&String> = commit.iter().collect();
        files.sort();
        files.dedup();
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let key = (files[i].clone(), files[j].clone());
                *pair_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    // 5. Calculate confidence and build entries
    let mut entries: Vec<CoChangeEntry> = pair_counts
        .into_iter()
        .filter_map(|((file_a, file_b), co_changes)| {
            let total_a = *file_counts.get(&file_a).unwrap_or(&0);
            let total_b = *file_counts.get(&file_b).unwrap_or(&0);
            let max_total = total_a.max(total_b);
            if max_total == 0 {
                return None;
            }

            let confidence = co_changes as f64 / max_total as f64;
            if confidence < min_confidence {
                return None;
            }

            // Filter by file if specified
            if let Some(filter) = filter_file
                && file_a != filter
                && file_b != filter
            {
                return None;
            }

            Some(CoChangeEntry {
                file_a,
                file_b,
                co_changes,
                total_changes_a: total_a,
                total_changes_b: total_b,
                confidence,
            })
        })
        .collect();

    // 6. Sort by confidence descending
    entries.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(CoChangeResult {
        entries,
        commits_analyzed,
    })
}
