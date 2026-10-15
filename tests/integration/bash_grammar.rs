use super::support::{TestRepo, cargo_bin};
use serde_json::Value;
use std::path::Path;
use tree_sitter::{Node, Parser, Tree};

fn bash_nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut nodes = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        nodes.push(node);
        pending.extend(node.children(&mut node.walk()));
    }
    nodes
}

fn parse_bash(source: &str) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .unwrap();
    parser.parse(source, None).unwrap()
}

fn bash_cli_json(command: &str, path: &Path, args: &[&str]) -> Value {
    let output = cargo_bin()
        .arg(command)
        .arg("--path")
        .arg(path)
        .args(args)
        .output()
        .expect("failed to run astro-sight");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("invalid JSON")
}

#[test]
fn bash_nested_parameter_expansion_keeps_parentheses_and_identifiers() {
    for operator in ["+", ":+", "-", ":-", "=", ":=", "?", ":?"] {
        let source = format!("echo \"${{label{operator} (${{label}}) }}\"\n");
        let tree = parse_bash(&source);
        assert!(
            !tree.root_node().has_error(),
            "{source}: {}",
            tree.root_node().to_sexp()
        );
        let nodes = bash_nodes(tree.root_node());
        assert!(
            nodes
                .iter()
                .all(|node| !node.is_error() && !node.is_missing())
        );
        let variables: Vec<_> = nodes
            .iter()
            .filter(|node| node.kind() == "variable_name")
            .map(|node| {
                (
                    node.utf8_text(source.as_bytes()).unwrap(),
                    node.start_position().column,
                )
            })
            .collect();
        assert_eq!(variables.len(), 2, "{}", tree.root_node().to_sexp());
        assert!(variables.contains(&("label", 8)));
        assert!(variables.contains(&("label", 8 + "label".len() + operator.len() + 4)));
        let closing_word = nodes
            .iter()
            .find(|node| {
                node.kind() == "word" && node.utf8_text(source.as_bytes()).unwrap() == ") "
            })
            .unwrap();
        let start = source.find(") ").unwrap();
        assert_eq!(closing_word.byte_range(), start..start + 2);
    }
}

#[test]
fn bash_nested_parameter_expansion_keeps_nested_delimiters_and_crlf_ranges() {
    for expression in [
        "${label:+ ((${label})) }",
        "${label:+ (${other:-${label}}) }",
        "${label:+ ($(printf '%s' \"${label}\")) }",
    ] {
        let source = format!("# 名前🙂\r\necho \"名前🙂 {expression}\"\r\n");
        let tree = parse_bash(&source);
        assert!(
            !tree.root_node().has_error(),
            "{source}: {}",
            tree.root_node().to_sexp()
        );
        let nodes = bash_nodes(tree.root_node());
        assert!(
            nodes
                .iter()
                .all(|node| !node.is_error() && !node.is_missing())
        );
        for node in nodes.iter().filter(|node| node.kind() == "variable_name") {
            let text = node.utf8_text(source.as_bytes()).unwrap();
            assert!(matches!(text, "label" | "other"));
            assert_eq!(node.start_position().row, 1);
            assert_eq!(
                node.start_position().column,
                node.start_byte() - "# 名前🙂\r\n".len()
            );
            assert_eq!(node.end_byte() - node.start_byte(), text.len());
        }
        assert_eq!(tree.root_node().byte_range(), 0..source.len());
    }
}

#[test]
fn bash_nested_parameter_expansion_cli_preserves_symbols_and_locations() {
    let source = "#!/bin/bash\nlabel=Terminal\nbefore() { :; }\nrender() {\n  echo \"名前🙂 ${label:+ (${label}) }\"\n}\nafter() { render; }\n";
    let repo = TestRepo::new();
    repo.write("nested.sh", source);
    let path = repo.path("nested.sh");
    for command in ["ast", "symbols"] {
        let args: &[&str] = if command == "ast" {
            &["--full", "--depth", "32", "--no-cache"]
        } else {
            &["--no-cache"]
        };
        let output = bash_cli_json(command, &path, args);
        assert!(
            output["diagnostics"]
                .as_array()
                .is_none_or(|items| items.iter().all(|item| item["severity"] != "error")),
            "{output}"
        );
        if command == "ast" {
            assert_eq!(
                output["hash"],
                blake3::hash(source.as_bytes()).to_hex().as_str()
            );
        }
    }
    let symbols = bash_cli_json("symbols", &path, &["--no-cache"]);
    for (name, line) in [("before", 2), ("render", 3), ("after", 6)] {
        assert!(symbols["symbols"].as_array().unwrap().iter().any(|symbol| {
            symbol["name"] == name && symbol["kind"] == "fn" && symbol["ln"] == line
        }));
    }
    let refs = repo.run_json("refs", &["--name", "render", "--max-results", "unlimited"]);
    let references = refs["refs"].as_array().unwrap();
    assert_eq!(references.len(), 2, "{refs}");
    for (kind, line, column) in [("def", 3, 0), ("ref", 6, 10)] {
        assert!(references.iter().any(|reference| {
            reference["path"] == "nested.sh"
                && reference["kind"] == kind
                && reference["ln"] == line
                && reference["col"] == column
        }));
    }
    let tree = parse_bash(source);
    let column = "  echo \"名前🙂 ${".len();
    for column in [column, column + "label:+ (${".len()] {
        assert!(bash_nodes(tree.root_node()).iter().any(|node| {
            node.kind() == "variable_name"
                && node.utf8_text(source.as_bytes()).unwrap() == "label"
                && node.start_position().row == 4
                && node.start_position().column == column
        }));
    }
    assert_eq!(std::fs::read(path).unwrap(), source.as_bytes());
}

#[test]
fn bash_nested_parameter_expansion_keeps_malformed_diagnostics() {
    for source in [
        "echo \"${label:+ (${label}) \"\n",
        "echo \"${label:+ (${label}) }\n",
        "echo \"$(echo value\"\n",
        "echo \"${label}\")\n",
        "{ echo \"${label}\"); }\n",
    ] {
        let tree = parse_bash(source);
        assert!(tree.root_node().has_error(), "{source}");
        assert!(
            bash_nodes(tree.root_node())
                .iter()
                .any(|node| node.is_error() || node.is_missing()),
            "{source}"
        );
        let repo = TestRepo::new();
        repo.write("broken.sh", source);
        for command in ["ast", "symbols"] {
            let output = bash_cli_json(command, &repo.path("broken.sh"), &["--no-cache"]);
            assert!(
                output["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["severity"] == "error"),
                "{output}"
            );
        }
    }
}

#[test]
fn bash_expansion_controls_preserve_baseline_ast() {
    // 変更前の文法が正常に解析する構文は、木全体を維持する。
    for (source, expected) in [
        (
            "{ echo \"${label}\"; }\n",
            "(program (compound_statement (command name: (command_name (word)) argument: (string (expansion (variable_name))))))",
        ),
        (
            "(echo \"${label}\")\n",
            "(program (subshell (command name: (command_name (word)) argument: (string (expansion (variable_name))))))",
        ),
        (
            "echo \"$(printf '%s' \"${label}\")\"\n",
            "(program (command name: (command_name (word)) argument: (string (command_substitution (command name: (command_name (word)) argument: (raw_string) argument: (string (expansion (variable_name))))))))",
        ),
        (
            "echo ${label:+(${label})}\n",
            "(program (command name: (command_name (word)) argument: (expansion (variable_name) (array (expansion (variable_name))))))",
        ),
        (
            "cat <<EOF\n${label}\nEOF\n",
            "(program (redirected_statement body: (command name: (command_name (word))) redirect: (heredoc_redirect (heredoc_start) (heredoc_body (expansion (variable_name)) (heredoc_content)) (heredoc_end))))",
        ),
        (
            "echo \"${label:+ ${other:-value}}\"\n",
            "(program (command name: (command_name (word)) argument: (string (expansion (variable_name) (expansion (variable_name) (word))))))",
        ),
        (
            "values=(\"${label}\" other)\n",
            "(program (variable_assignment name: (variable_name) value: (array (string (expansion (variable_name))) (word))))",
        ),
        (
            "echo \"${label:+ (word) }\"\n",
            "(program (command name: (command_name (word)) argument: (string (expansion (variable_name) (word)))))",
        ),
        (
            "echo ' ) ${label:+ (${label}) } '\n",
            "(program (command name: (command_name (word)) argument: (raw_string)))",
        ),
    ] {
        let tree = parse_bash(source);
        assert!(!tree.root_node().has_error(), "{source}");
        assert_eq!(tree.root_node().to_sexp(), expected, "{source}");
    }
}
