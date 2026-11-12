//! MCP サーバーモード (stdio JSON-RPC) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::Stdio;

#[test]
fn mcp_initialize() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = cargo_bin()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
    )
    .unwrap();
    // サーバーを終了させるために stdin を閉じる
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");

    let stdout = String::from_utf8(output.stdout).unwrap();
    // 1行目は initialize レスポンス
    let first_line = stdout.lines().next().expect("should have output");
    let json: serde_json::Value =
        serde_json::from_str(first_line).expect("should be valid JSON-RPC");
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["id"], 1);
    assert_eq!(json["result"]["serverInfo"]["name"], "astro-sight");
    assert_eq!(json["result"]["serverInfo"]["version"], PKG_VERSION);
    assert!(json["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn mcp_tools_list() {
    let stdout = mcp_send_after_init(&[r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#]);

    let tools_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("tools/list レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(tools_line).expect("valid JSON-RPC");
    let tools = json["result"]["tools"]
        .as_array()
        .expect("tools 配列が必要");
    assert!(
        tools.len() >= 12,
        "12ツール以上が必要、実際: {}",
        tools.len()
    );
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "metrics_analyze",
        "ast_extract",
        "symbols_extract",
        "calls_extract",
        "refs_search",
        "refs_batch_search",
        "context_analyze",
        "imports_extract",
        "lint",
        "sequence_diagram",
        "cochange_analyze",
        "doctor",
    ] {
        assert!(
            tool_names.contains(&expected),
            "ツール '{expected}' が tools/list に含まれるべき"
        );
    }
}

#[test]
fn mcp_tools_call_symbols() {
    let stdout = mcp_send_after_init(&[
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"symbols_extract","arguments":{"path":"tests/fixtures/sample.py"}}}"#,
    ]);

    let result_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("tools/call レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    let content = json["result"]["content"]
        .as_array()
        .expect("content 配列が必要");
    assert!(!content.is_empty(), "content が空であってはならない");
    let text = content[0]["text"].as_str().expect("text フィールドが必要");
    let symbols: serde_json::Value =
        serde_json::from_str(text).expect("symbols JSON がパース可能であるべき");
    assert!(
        symbols["symbols"].as_array().is_some(),
        "symbols 配列が必要"
    );
}

/// MCP の `lint` ツールは、テキストのパターンと tree-sitter のクエリの規則で一致を返す。
/// 不正な severity はその要求だけをエラーにし、同じ接続の後続の要求は処理を続ける。
/// 11 ツールのうち `lint` だけ MCP 経由の呼び出しを確かめるテストが無かった。
#[test]
fn mcp_tools_call_lint_reports_matches_and_rejects_invalid_severity() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/main.rs",
        "fn helper() -> Option<u32> { Some(1) }\nfn main() { let v = helper().unwrap(); println!(\"{v}\"); }\n",
    );
    let lint_call = |id: u64, severity: &str| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "lint",
                "arguments": {
                    "path": "src/main.rs",
                    "rules": [
                        {
                            "id": "no-unwrap",
                            "language": "rust",
                            "severity": severity,
                            "message": "unwrap を使わない",
                            "pattern": "unwrap"
                        },
                        {
                            "id": "find-functions",
                            "language": "rust",
                            "severity": "info",
                            "message": "関数",
                            "query": "(function_item name: (identifier) @fn_name)"
                        }
                    ]
                }
            }
        })
        .to_string()
    };
    let valid = lint_call(2, "warning");
    let invalid = lint_call(3, "fatal");
    let after_error = lint_call(4, "error");
    let stdout = mcp_send_after_init_with(
        &[],
        Some(repo.root()),
        &[valid.as_str(), invalid.as_str(), after_error.as_str()],
    );
    let response = |id: u64| -> serde_json::Value {
        let line = stdout
            .lines()
            .find(|line| line.contains(&format!("\"id\":{id}")))
            .unwrap_or_else(|| panic!("id={id} の応答が必要: {stdout}"));
        serde_json::from_str(line).expect("valid JSON-RPC")
    };
    let rule_ids = |json: &serde_json::Value| -> Vec<String> {
        let text = json["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("text フィールドが必要: {json}"));
        let result: serde_json::Value = serde_json::from_str(text).expect("lint の結果は JSON");
        assert_eq!(result["lang"], "rust", "{result}");
        let mut ids: Vec<String> = result["matches"]
            .as_array()
            .expect("matches 配列が必要")
            .iter()
            .map(|m| m["rule_id"].as_str().expect("rule_id").to_string())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };

    assert_eq!(rule_ids(&response(2)), ["find-functions", "no-unwrap"]);
    let rejected = response(3);
    let message = rejected["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("不正な severity はエラー応答: {rejected}"));
    assert!(
        message.contains("Invalid severity 'fatal'") && message.contains("no-unwrap"),
        "{message}"
    );
    assert_eq!(rule_ids(&response(4)), ["find-functions", "no-unwrap"]);
}

#[test]
fn mcp_tools_call_refs_search() {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "refs_search",
            "arguments": {
                "name": "Config",
                "dir": "tests/fixtures",
                "glob": "**/*.py"
            }
        }
    })
    .to_string();
    let stdout = mcp_send_after_init(&[&request]);

    let result_line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .expect("refs_search レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    let content = json["result"]["content"]
        .as_array()
        .expect("content 配列が必要");
    let text = content[0]["text"].as_str().expect("text フィールドが必要");
    let refs: serde_json::Value =
        serde_json::from_str(text).expect("refs JSON がパース可能であるべき");

    assert_eq!(refs["symbol"], "Config");
    assert!(
        refs["refs"]
            .as_array()
            .is_some_and(|references| references.iter().any(|reference| {
                reference["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("sample.py"))
            })),
        "sample.py 内の Config 参照が必要: {refs}"
    );
}

#[test]
fn mcp_tools_call_refs_batch_search_keeps_array_response() {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "refs_batch_search",
            "arguments": {
                "names": ["Config", "UserService"],
                "dir": "tests/fixtures",
                "glob": "**/*.py"
            }
        }
    })
    .to_string();
    let stdout = mcp_send_after_init(&[&request]);

    let result_line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .expect("refs_batch_search レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    let text = json["result"]["content"][0]["text"]
        .as_str()
        .expect("text フィールドが必要");
    let refs: serde_json::Value = serde_json::from_str(text).expect("refs JSON が必要");
    assert!(
        refs.is_array(),
        "batch response must retain its array shape: {refs}"
    );
    assert_eq!(refs.as_array().unwrap().len(), 2);
}

#[test]
fn mcp_tools_call_context_analyze() {
    let diff = "\
diff --git a/tests/fixtures/sample.py b/tests/fixtures/sample.py\n\
--- a/tests/fixtures/sample.py\n\
+++ b/tests/fixtures/sample.py\n\
@@ -28,1 +28,1 @@\n\
-def create_config(path: str) -> Config:\n\
+def create_config(path: str, strict: bool = False) -> Config:\n";
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "context_analyze",
            "arguments": {
                "diff": diff,
                "dir": "."
            }
        }
    })
    .to_string();
    let stdout = mcp_send_after_init(&[&request]);

    let result_line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .expect("context_analyze レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    let content = json["result"]["content"]
        .as_array()
        .expect("content 配列が必要");
    let text = content[0]["text"].as_str().expect("text フィールドが必要");
    let context: serde_json::Value =
        serde_json::from_str(text).expect("context JSON がパース可能であるべき");

    assert!(
        context["changes"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| {
                change["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("tests/fixtures/sample.py"))
            })),
        "sample.py の変更影響が必要: {context}"
    );
}

#[test]
fn mcp_tools_call_path_traversal() {
    let stdout = mcp_send_after_init(&[
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"symbols_extract","arguments":{"path":"/etc/hosts"}}}"#,
    ]);

    let result_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("エラーレスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    assert!(
        json["error"].is_object(),
        "パストラバーサルはエラーを返すべき: {json}"
    );
}

// ---- MCP sandbox fail-closed テスト ----

#[test]
fn mcp_sandbox_fail_closed_with_file_workspace() {
    // MCP サーバーはファイルを workspace root として受け付けない。
    // AppService::sandboxed がファイルを拒否するので、
    // AstroSightServer::new() 相当のロジックが fail-closed であることを
    // AppService レベルで確認。
    let dir = tempfile::TempDir::new().unwrap();
    let file_path = dir.path().join("not_a_dir.txt");
    std::fs::write(&file_path, "content").unwrap();

    let result = astro_sight::service::AppService::sandboxed(file_path);
    assert!(result.is_err(), "ファイルを workspace root にできないべき");
}

#[test]
fn mcp_sandbox_fail_closed_with_nonexistent_dir() {
    // 存在しないディレクトリでサンドボックスは生成できない
    let result = astro_sight::service::AppService::sandboxed(std::path::PathBuf::from(
        "/nonexistent/path/that/does/not/exist",
    ));
    assert!(
        result.is_err(),
        "存在しないディレクトリで sandbox は生成できないべき"
    );
}

// ---- MCP: refs の予算は tool result と同じ描画で採寸する ----

/// `output::estimated_tokens` と同じ換算 (文字数 + 4 × 改行数 を 3 で割って切り上げ)。
fn estimated_tokens(text: &str) -> usize {
    let newlines = text.bytes().filter(|b| *b == b'\n').count();
    (text.chars().count() + 4 * newlines).div_ceil(3)
}

/// `tool` を 1 回呼び、tool result の text を返す。
fn mcp_tool_text(
    global_args: &[&str],
    cwd: &std::path::Path,
    tool: &str,
    arguments: serde_json::Value,
) -> String {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
    .to_string();
    let stdout = mcp_send_after_init_with(global_args, Some(cwd), &[&request]);
    let result_line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .unwrap_or_else(|| panic!("{tool} のレスポンスが必要: {stdout}"));
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    json["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("text フィールドが必要: {json}"))
        .to_string()
}

/// text に含まれる参照の件数。JSON (単体 / 配列) と TOON (`refs[N]` ヘッダの合計) の両方を読む。
fn shown_refs(text: &str) -> usize {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let results = match value.as_array() {
            Some(items) => items.clone(),
            None => vec![value],
        };
        return results
            .iter()
            .map(|r| r["refs"].as_array().map_or(0, Vec::len))
            .sum();
    }
    text.match_indices("refs[")
        .map(|(i, _)| {
            let digits: String = text[i + "refs[".len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits
                .parse::<usize>()
                .expect("TOON の refs ヘッダには件数がある")
        })
        .sum()
}

/// MCP の `token_budget` は **実際に返す text** (`--format` / `--pretty` に従う) で採寸する。
///
/// 旧実装は予算の採寸だけ compact JSON に固定していたため、`--pretty mcp` では予算の
/// 約 2 倍の text を `budget_exceeded` なしで返し、逆に 1 件あたりが軽い TOON では予算を
/// 使い切らずに compact JSON と同じ件数で打ち切っていた (同条件の CLI は実描画で採寸する)。
/// 単体 (`refs_search`) とバッチ (`refs_batch_search`) の両経路を確かめる。
#[test]
fn mcp_refs_budget_is_measured_on_the_rendered_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut src = String::from("pub fn alpha() {}\npub fn bravo() {}\npub fn charlie() {}\n");
    for i in 0..120 {
        src.push_str(&format!(
            "pub fn caller{i}() {{ alpha(); bravo(); charlie(); }}\n"
        ));
    }
    std::fs::write(dir.path().join("a.rs"), &src).expect("write");
    let budget = 1500;

    let calls = [
        (
            "refs_search",
            serde_json::json!({ "name": "alpha", "dir": ".", "token_budget": budget }),
        ),
        (
            "refs_batch_search",
            serde_json::json!({
                "names": ["alpha", "bravo", "charlie"],
                "dir": ".",
                "token_budget": budget
            }),
        ),
    ];
    for (tool, arguments) in calls {
        let text_for = |args: &[&str]| mcp_tool_text(args, dir.path(), tool, arguments.clone());

        // 対照: compact JSON (既定) は従来どおり予算内。
        let compact = text_for(&[]);
        assert!(
            estimated_tokens(&compact) <= budget,
            "{tool} compact: {} tokens",
            estimated_tokens(&compact)
        );
        let compact_shown = shown_refs(&compact);
        assert!(compact_shown > 0, "{tool}: 予算内で参照を返す");

        let pretty = text_for(&["--pretty"]);
        let toon = text_for(&["--format", "toon"]);
        for (label, text) in [("pretty", &pretty), ("toon", &toon)] {
            assert!(
                estimated_tokens(text) <= budget || text.contains("budget_exceeded"),
                "{tool} {label}: 予算 {budget} に対し {} tokens を申告なしで返した",
                estimated_tokens(text)
            );
        }

        // 1 件あたりが compact より重い pretty は同じ予算で出せる件数が減り、軽い TOON は
        // 増える (採寸が compact JSON 固定のままだと、どちらも compact と同じ件数になる)。
        assert!(
            shown_refs(&pretty) < compact_shown,
            "{tool}: pretty={} compact={compact_shown}",
            shown_refs(&pretty)
        );
        assert!(
            shown_refs(&toon) > compact_shown,
            "{tool}: toon={} compact={compact_shown}",
            shown_refs(&toon)
        );
    }
}

/// MCP の `refs_search` も、検索できる入力が無い範囲を成功の空結果にせずエラーにする。
#[test]
fn mcp_refs_search_rejects_scope_without_searchable_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("sample.applescript"),
        "on greetUser()\nend greetUser\n",
    )
    .expect("write");
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "refs_search",
            "arguments": { "name": "greetUser", "dir": "." }
        }
    })
    .to_string();
    let stdout = mcp_send_after_init_with(&[], Some(dir.path()), &[&request]);
    let line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .unwrap_or_else(|| panic!("refs_search のレスポンスが必要: {stdout}"));
    let json: serde_json::Value = serde_json::from_str(line).expect("valid JSON-RPC");
    let message = json["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("{json}"));
    assert!(message.contains("UNSUPPORTED_LANGUAGE"), "{message}");
    assert!(json.get("result").is_none(), "{json}");
}

/// cochange は引数を省略したとき、CLI・session・MCP の `cochange_analyze` が同じ既定値で
/// 解析する。
///
/// 旧実装は MCP だけ `max_files_per_commit` の既定を 30 と書き写しており、CLI と
/// session の既定 (100) を上げたときに取り残されていた。31〜100 ファイルを変える
/// コミットが MCP でだけ丸ごと捨てられ、同じリポジトリで共変更の結果が食い違う。
/// フィクスチャは a.rs と b.rs を毎回 40 ファイルのコミットで一緒に変えるので、
/// 上限 30 では証拠が 1 件も残らない (対照として結果が空でないことも確かめる)。
#[test]
fn cochange_defaults_match_across_cli_session_and_mcp() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    for round in 0..4 {
        repo.write("src/a.rs", format!("pub fn a() -> u32 {{ {round} }}\n"));
        repo.write("src/b.rs", format!("pub fn b() -> u32 {{ {round} }}\n"));
        for filler in 0..38 {
            repo.write(
                format!("src/f{filler}.rs"),
                format!("pub fn f{filler}() -> u32 {{ {round} }}\n"),
            );
        }
        repo.commit_all(&format!("round {round}"));
    }

    let entries_of = |json: &serde_json::Value| -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = json["entries"]
            .as_array()
            .unwrap_or_else(|| panic!("entries 配列が必要: {json}"))
            .iter()
            .map(|entry| {
                (
                    entry["file_a"].as_str().unwrap_or_default().to_string(),
                    entry["file_b"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        entries.sort();
        entries
    };

    let cli = repo.run_json("cochange", &["--paths", "src/a.rs"]);
    let cli_entries = entries_of(&cli);
    assert!(
        !cli_entries.is_empty(),
        "CLI の既定値では 40 ファイルのコミットも証拠に数える: {cli}"
    );

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "cochange_analyze",
            "arguments": { "dir": ".", "source_files": ["src/a.rs"] }
        }
    })
    .to_string();
    let stdout = mcp_send_after_init_with(&[], Some(repo.root()), &[&request]);
    let line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .unwrap_or_else(|| panic!("cochange_analyze のレスポンスが必要: {stdout}"));
    let json: serde_json::Value = serde_json::from_str(line).expect("valid JSON-RPC");
    let text = json["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("text フィールドが必要: {json}"));
    let mcp: serde_json::Value = serde_json::from_str(text).expect("cochange JSON");
    assert_eq!(
        entries_of(&mcp),
        cli_entries,
        "MCP と CLI は既定値で同じ共変更を返す: mcp={mcp} cli={cli}"
    );

    let session = {
        use std::io::Write;
        let mut child = cargo_bin()
            .arg("session")
            .env("ASTRO_SIGHT_WORKSPACE", repo.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("failed to spawn session");
        writeln!(
            child.stdin.as_mut().expect("stdin"),
            r#"{{"command":"cochange","dir":".","source_files":["src/a.rs"]}}"#
        )
        .expect("write session request");
        let output = child.wait_with_output().expect("failed to wait session");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        serde_json::from_str::<serde_json::Value>(stdout.trim())
            .unwrap_or_else(|_| panic!("session の cochange 応答が必要: {stdout}"))
    };
    assert_eq!(
        entries_of(&session),
        cli_entries,
        "session と CLI は既定値で同じ共変更を返す: session={session} cli={cli}"
    );
}
