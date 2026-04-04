use anyhow::Result;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::lint::{PatternMatch, Rule};

/// YAML ファイルからルールを読み込む。
pub fn load_rules_from_file(path: &str) -> Result<Vec<Rule>> {
    let content = std::fs::read_to_string(path)?;
    let rules: Vec<Rule> = serde_yaml::from_str(&content)?;
    Ok(rules)
}

/// ディレクトリ内の全 YAML ファイルからルールを読み込む。
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

/// 指定ルールでファイルを lint する。
/// (matches, warnings) を返す。warnings にはスキップ・無効ルールの情報を含む。
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
        // 異なる言語のルールをスキップ
        if rule.language != lang_name {
            continue;
        }

        // バリデーション: query または pattern のいずれかが必須
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
            // モード 1: tree-sitter クエリ
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
            // モード 2: identifier ノードへのテキストパターンマッチ
            collect_pattern_matches(root, source, pattern, rule, &mut matches);
        }
    }

    Ok((matches, warnings))
}

/// AST を再帰走査し、identifier ノードをテキストパターンと照合する。
fn collect_pattern_matches(
    node: Node<'_>,
    source: &[u8],
    pattern: &str,
    rule: &Rule,
    matches: &mut Vec<PatternMatch>,
) {
    let kind = node.kind();
    // identifier 系ノードをチェック
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::lint::Severity;

    fn parse_rust(source: &str) -> (tree_sitter::Tree, LangId) {
        let lang_id = LangId::Rust;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang_id.ts_language()).unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        (tree, lang_id)
    }

    fn make_rule(
        id: &str,
        language: &str,
        severity: Severity,
        query: Option<&str>,
        pattern: Option<&str>,
    ) -> Rule {
        Rule {
            id: id.to_string(),
            language: language.to_string(),
            severity,
            message: format!("{id} matched"),
            query: query.map(|s| s.to_string()),
            pattern: pattern.map(|s| s.to_string()),
        }
    }

    // --- YAML ルール読み込みテスト ---

    /// YAML ファイルからルールを正常に読み込む
    #[test]
    fn load_rules_from_yaml_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.yaml");
        std::fs::write(
            &path,
            r#"
- id: no-unwrap
  language: rust
  severity: warning
  message: "avoid unwrap()"
  pattern: "unwrap"
"#,
        )
        .unwrap();
        let rules = load_rules_from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "no-unwrap");
        assert_eq!(rules[0].pattern.as_deref(), Some("unwrap"));
    }

    /// ディレクトリ内の複数 YAML ファイルからルールを読み込む
    #[test]
    fn load_rules_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.yaml"),
            r#"
- id: rule-a
  language: rust
  severity: info
  message: "rule a"
  pattern: "foo"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.yml"),
            r#"
- id: rule-b
  language: rust
  severity: error
  message: "rule b"
  pattern: "bar"
"#,
        )
        .unwrap();
        // .txt は無視される
        std::fs::write(dir.path().join("c.txt"), "not yaml").unwrap();

        let rules = load_rules_from_dir(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(rules.len(), 2);
        let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"rule-a"));
        assert!(ids.contains(&"rule-b"));
    }

    /// 存在しないディレクトリでは空リストを返す
    #[test]
    fn load_rules_from_nonexistent_dir() {
        let rules = load_rules_from_dir("/nonexistent/dir").unwrap();
        assert!(rules.is_empty());
    }

    // --- tree-sitter クエリモードのテスト ---

    /// tree-sitter クエリで関数名をマッチする
    #[test]
    fn lint_query_mode_matches_function() {
        let source = "fn dangerous_operation() {}\nfn safe_fn() {}";
        let (tree, lang_id) = parse_rust(source);
        let rule = make_rule(
            "no-dangerous",
            "rust",
            Severity::Warning,
            Some("(function_item name: (identifier) @fn.name)"),
            None,
        );
        let (matches, warnings) =
            lint_file(tree.root_node(), source.as_bytes(), lang_id, &[rule]).unwrap();
        assert!(warnings.is_empty());
        // 2つの関数名がマッチ
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches.iter().map(|m| m.matched_text.as_str()).collect();
        assert!(names.contains(&"dangerous_operation"));
        assert!(names.contains(&"safe_fn"));
    }

    /// 無効な tree-sitter クエリでは warning が出る
    #[test]
    fn lint_invalid_query_produces_warning() {
        let source = "fn foo() {}";
        let (tree, lang_id) = parse_rust(source);
        let rule = make_rule(
            "bad-query",
            "rust",
            Severity::Error,
            Some("(invalid_node_that_does_not_exist @name)"),
            None,
        );
        let (matches, warnings) =
            lint_file(tree.root_node(), source.as_bytes(), lang_id, &[rule]).unwrap();
        assert!(matches.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("invalid tree-sitter query"));
    }

    // --- テキストパターンモードのテスト ---

    /// identifier のテキストパターンマッチ
    #[test]
    fn lint_pattern_mode_matches_identifier() {
        let source = "fn foo_unsafe() {}\nfn bar() {}";
        let (tree, lang_id) = parse_rust(source);
        let rule = make_rule(
            "no-unsafe-name",
            "rust",
            Severity::Warning,
            None,
            Some("unsafe"),
        );
        let (matches, warnings) =
            lint_file(tree.root_node(), source.as_bytes(), lang_id, &[rule]).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].matched_text, "foo_unsafe");
    }

    // --- バリデーションテスト ---

    /// query と pattern の両方が指定された場合に warning が出る
    #[test]
    fn lint_both_query_and_pattern_warns() {
        let source = "fn foo() {}";
        let (tree, lang_id) = parse_rust(source);
        let rule = Rule {
            id: "both".to_string(),
            language: "rust".to_string(),
            severity: Severity::Info,
            message: "both".to_string(),
            query: Some("(function_item name: (identifier) @fn.name)".to_string()),
            pattern: Some("foo".to_string()),
        };
        let (_matches, warnings) =
            lint_file(tree.root_node(), source.as_bytes(), lang_id, &[rule]).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("mutually exclusive"));
    }

    /// query も pattern もない場合はスキップされる
    #[test]
    fn lint_neither_query_nor_pattern_skips() {
        let source = "fn foo() {}";
        let (tree, lang_id) = parse_rust(source);
        let rule = make_rule("empty", "rust", Severity::Error, None, None);
        let (matches, warnings) =
            lint_file(tree.root_node(), source.as_bytes(), lang_id, &[rule]).unwrap();
        assert!(matches.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("must have either query or pattern"));
    }

    /// 異なる言語のルールはスキップされる
    #[test]
    fn lint_skips_different_language_rules() {
        let source = "fn foo() {}";
        let (tree, lang_id) = parse_rust(source);
        let rule = make_rule(
            "js-rule",
            "javascript",
            Severity::Warning,
            None,
            Some("foo"),
        );
        let (matches, warnings) =
            lint_file(tree.root_node(), source.as_bytes(), lang_id, &[rule]).unwrap();
        assert!(matches.is_empty());
        assert!(warnings.is_empty());
    }

    /// パターンマッチの行番号・列番号が正しい
    #[test]
    fn lint_match_position_is_correct() {
        let source = "fn foo() {}\nfn bar_target() {}";
        let (tree, lang_id) = parse_rust(source);
        let rule = make_rule("find-target", "rust", Severity::Info, None, Some("target"));
        let (matches, _) =
            lint_file(tree.root_node(), source.as_bytes(), lang_id, &[rule]).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, 1); // 2行目（0-indexed）
        assert_eq!(matches[0].matched_text, "bar_target");
    }
}
