use anyhow::Result;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::lint::{PatternMatch, Rule};

/// Load rules from a YAML file.
pub fn load_rules_from_file(path: &str) -> Result<Vec<Rule>> {
    let content = std::fs::read_to_string(path)?;
    let rules: Vec<Rule> = serde_yaml::from_str(&content)?;
    Ok(rules)
}

/// Load rules from all YAML files in a directory.
pub fn load_rules_from_dir(dir: &str) -> Result<Vec<Rule>> {
    let mut all_rules = Vec::new();
    let dir_path = std::path::Path::new(dir);
    if !dir_path.is_dir() {
        return Ok(all_rules);
    }

    let mut entries: Vec<_> = std::fs::read_dir(dir_path)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("yaml" | "yml")
            )
        })
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let content = std::fs::read_to_string(entry.path())?;
        let rules: Vec<Rule> = serde_yaml::from_str(&content)?;
        all_rules.extend(rules);
    }
    Ok(all_rules)
}

/// Lint a file against the given rules.
/// Returns (matches, warnings). Warnings contain info about skipped/invalid rules.
pub fn lint_file(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    rules: &[Rule],
) -> Result<(Vec<PatternMatch>, Vec<String>)> {
    let lang_name = lang_id.to_string();
    let language = lang_id.ts_language();
    let mut matches = Vec::new();
    let mut warnings = Vec::new();

    for rule in rules {
        // Skip rules for other languages
        if rule.language != lang_name {
            continue;
        }

        // Validate: must have exactly one of query or pattern
        if rule.query.is_some() && rule.pattern.is_some() {
            warnings.push(format!(
                "Rule '{}': query and pattern are mutually exclusive; using query",
                rule.id
            ));
        }
        if rule.query.is_none() && rule.pattern.is_none() {
            warnings.push(format!(
                "Rule '{}': must have either query or pattern; skipped",
                rule.id
            ));
            continue;
        }

        if let Some(query_src) = &rule.query {
            // Mode 1: tree-sitter query
            match Query::new(&language, query_src) {
                Ok(query) => {
                    let mut cursor = QueryCursor::new();
                    let mut query_matches = cursor.matches(&query, root, source);

                    while let Some(m) = query_matches.next() {
                        for capture in m.captures {
                            let node = capture.node;
                            let matched_text = node.utf8_text(source).unwrap_or("").to_string();

                            matches.push(PatternMatch {
                                rule_id: rule.id.clone(),
                                severity: rule.severity,
                                message: rule.message.clone(),
                                line: node.start_position().row,
                                column: node.start_position().column,
                                matched_text,
                            });
                        }
                    }
                }
                Err(e) => {
                    warnings.push(format!(
                        "Rule '{}': invalid tree-sitter query: {e}; skipped",
                        rule.id
                    ));
                    continue;
                }
            }
        } else if let Some(pattern) = &rule.pattern {
            // Mode 2: text pattern matching on identifier nodes
            collect_pattern_matches(root, source, pattern, rule, &mut matches);
        }
    }

    Ok((matches, warnings))
}

/// Recursively walk the AST and match identifier nodes against a text pattern.
fn collect_pattern_matches(
    node: Node<'_>,
    source: &[u8],
    pattern: &str,
    rule: &Rule,
    matches: &mut Vec<PatternMatch>,
) {
    let kind = node.kind();
    // Check identifier-like nodes
    let is_identifier = kind == "identifier"
        || kind == "field_identifier"
        || kind == "type_identifier"
        || kind == "property_identifier"
        || kind == "simple_identifier"
        || kind == "word"
        || kind == "name";

    if is_identifier
        && let Ok(text) = node.utf8_text(source)
        && text.contains(pattern)
    {
        matches.push(PatternMatch {
            rule_id: rule.id.clone(),
            severity: rule.severity,
            message: rule.message.clone(),
            line: node.start_position().row,
            column: node.start_position().column,
            matched_text: text.to_string(),
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_pattern_matches(child, source, pattern, rule, matches);
    }
}
