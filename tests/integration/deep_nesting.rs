//! 深い入れ子の構文木で、解析がスタックを使い切ってプロセスごと落ちないことの統合テスト。
//!
//! 括弧が数万段入れ子になった式や長い `else if` / `||` の連鎖は構文として正しく、
//! 生成コードでは現実に現れる。構文木を子 1 段ごとに再帰で辿る処理があると、こうした
//! ファイル 1 つで CLI・session・MCP サーバーが exit 134 (stack overflow) で落ち、
//! 同じ要求に含まれる他のファイルの結果も失われていた。
//!
//! スタック溢れはプロセスごと abort するので、テストはすべて子プロセスで実行する
//! (テストプロセスは巻き込まれない)。exit 0 だけでなく、深い部分の**後ろ**の参照が
//! 返ること、深いファイルからしか使われないシンボルを dead にしないことまで確かめる。

use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::support::*;

/// 子プロセスを期限付きで実行する。stdout / stderr は並行して読み、期限を過ぎたら
/// kill して失敗にする (無限ループの退行でテストが止まらないように)。
fn run_bounded(mut command: Command, stdin: Option<String>) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn astro-sight");
    let mut child_stdin = child.stdin.take().expect("stdin");
    let writer = std::thread::spawn(move || {
        if let Some(input) = stdin {
            // 子が先に落ちた場合の書き込みエラーは、終了状態の検査で検出する。
            let _ = child_stdin.write_all(input.as_bytes());
        }
    });
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("astro-sight did not finish within the deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    writer.join().expect("stdin writer");
    Output {
        status,
        stdout: out_reader.join().expect("stdout reader"),
        stderr: err_reader.join().expect("stderr reader"),
    }
}

/// MCP サーバーを `cwd` で起動し、initialize の応答を待ってから `requests` を送る。
/// `expected_ids` の応答がそろう (または期限を過ぎる) まで stdin を閉じない。
///
/// 応答を待たずに stdin を閉じると、サーバーは処理中の要求を残したまま終了する。
/// 負荷の高い環境では深い入力の要求が終わる前に閉じてしまい、テストの目的
/// (落ちずに応答し続けること) と関係なく応答が欠けていた。サーバーがスタック溢れで
/// 落ちた場合は出力がその時点で閉じるので、欠けた応答として検出できる。
fn mcp_requests_waiting_for_responses(
    cwd: &std::path::Path,
    requests: &[String],
    expected_ids: &[u64],
) -> String {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let mut child = cargo_bin()
        .arg("mcp")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(300);
    let mut lines: Vec<String> = Vec::new();
    // 期限内に `id` の応答が届くまで読む。出力が閉じたら (サーバーの終了) false。
    let wait_for = |id: u64, lines: &mut Vec<String>| -> bool {
        let needle = format!("\"id\":{id}");
        loop {
            if lines.iter().any(|line| line.contains(&needle)) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(line) => lines.push(line),
                Err(_) => return false,
            }
        }
    };

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
    )
    .expect("write initialize");
    stdin.flush().expect("flush");
    assert!(
        wait_for(1, &mut lines),
        "initialize の応答が必要: {lines:?}"
    );
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .expect("write initialized");
    for request in requests {
        writeln!(stdin, "{request}").expect("write request");
    }
    stdin.flush().expect("flush");
    for &id in expected_ids {
        if !wait_for(id, &mut lines) {
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    reader.join().expect("reader thread");
    lines.extend(rx.try_iter());
    lines.join("\n")
}

/// 正常終了したことを確かめ、stdout を返す。
fn assert_finished(label: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{label}: {:?} stderr={}",
        output.status,
        stderr.chars().take(400).collect::<String>()
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn parse_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("invalid JSON line"))
        .collect()
}

fn nested(open: &str, inner: &str, close: &str, depth: usize) -> String {
    format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
}

/// 関数の中の深い括弧・12,000 段の `else if`・20,000 項の `||` (F1: 循環的複雑度)。
fn write_deep_functions(repo: &TestRepo) {
    repo.write(
        "fn_parens.js",
        format!(
            "function f() {{ return {}; }}\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let mut elif = String::from("function g(code) { if (code === 0) { return 0; }");
    for i in 1..12_000 {
        elif.push_str(&format!(" else if (code === {i}) {{ return {i}; }}"));
    }
    elif.push_str(" return -1; }\n");
    repo.write("elif.js", elif);
    let terms: Vec<String> = (0..20_000).map(|i| format!("a === {i}")).collect();
    repo.write(
        "or.js",
        format!("function isKw(a) {{ return {}; }}\n", terms.join(" || ")),
    );
    repo.write("ok.js", "export function okay(x) { return x ? 1 : 2; }\n");
}

/// symbols の `--paths` と `--dir` が深い関数本体で落ちず、循環的複雑度も数えきる。
#[test]
fn symbols_handles_deep_function_bodies() {
    let repo = TestRepo::new();
    write_deep_functions(&repo);
    let cx_of = |records: &[serde_json::Value], name: &str| -> Option<u64> {
        records
            .iter()
            .flat_map(|r| r["symbols"].as_array().cloned().unwrap_or_default())
            .find(|s| s["name"] == name)
            .and_then(|s| s["cx"].as_u64())
    };

    for file in ["fn_parens.js", "elif.js", "or.js"] {
        let mut command = cargo_bin();
        command.current_dir(repo.root()).args([
            "symbols",
            "--no-cache",
            "--paths",
            &format!("{file},ok.js"),
        ]);
        let stdout = assert_finished(file, &run_bounded(command, None));
        let records = parse_lines(&stdout);
        assert_eq!(records.len(), 2, "{file}: 2 ファイル分の結果: {stdout}");
        assert_eq!(
            cx_of(&records, "okay"),
            Some(2),
            "{file}: 同じ要求の正常なファイル"
        );
    }

    let mut command = cargo_bin();
    command
        .current_dir(repo.root())
        .args(["symbols", "--no-cache", "--dir", "."]);
    let stdout = assert_finished("symbols --dir", &run_bounded(command, None));
    let records = parse_lines(&stdout);
    assert_eq!(
        cx_of(&records, "g"),
        Some(12_001),
        "else if の連鎖を全段数える"
    );
    assert_eq!(cx_of(&records, "f"), Some(1));
    assert_eq!(cx_of(&records, "okay"), Some(2));
}

/// C++ の目印 (コメント中の `::`) を持つ深い `.h` で、言語判定 (C と C++ の
/// parse error の比較) が落ちない (F2)。refs の単一・バッチ検索と dead-code の
/// 件数集計の入口で毎回通る。深い初期化子の後ろの呼び出しも数える。
#[test]
fn refs_and_dead_code_handle_deep_cpp_marked_header() {
    let repo = TestRepo::new();
    repo.write(
        "deep.h",
        format!(
            "// see foo::bar\nint deep_value = {};\nint after_deep(void) {{ return helper(); }}\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    repo.write(
        "ok.c",
        "int helper(void) { return 1; }\nint use_deep(void) { return deep_value; }\n",
    );

    let mut command = cargo_bin();
    command
        .args(["refs", "--name", "helper", "--dir"])
        .arg(repo.root());
    let stdout = assert_finished("refs --name", &run_bounded(command, None));
    let refs = parse_lines(&stdout);
    assert!(
        refs[0]["refs"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r["path"] == "deep.h" && r["ln"] == 2)),
        "深い初期化子の後ろの呼び出しを返す: {stdout}"
    );

    let mut command = cargo_bin();
    command
        .args(["refs", "--names", "deep_value,helper", "--dir"])
        .arg(repo.root());
    let stdout = assert_finished("refs --names", &run_bounded(command, None));
    assert_eq!(parse_lines(&stdout).len(), 2, "{stdout}");

    let mut command = cargo_bin();
    command.args(["dead-code", "--dir"]).arg(repo.root());
    let stdout = assert_finished("dead-code", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("dead-code JSON");
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !dead.contains(&"helper"),
        "深いヘッダからしか呼ばれない helper は dead ではない: {dead:?}"
    );
}

/// Rust: 検索名が closure の引数に束縛されているとき、ファイル内の宣言を調べる処理が
/// 深い式で落ちない (F3-a)。closure の束縛は数えず、深い式の後ろの呼び出しは数える。
#[test]
fn refs_handles_closure_binding_check_in_deep_rust_file() {
    let repo = TestRepo::new();
    repo.write(
        "lib.rs",
        format!(
            "pub fn tail() -> u8 {{ 1 }}\npub fn run(v: &[u8]) -> usize {{ v.iter().map(|tail| *tail).count() }}\npub const DEEP: u32 = {};\npub fn after() -> u8 {{ tail() }}\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .args(["refs", "--name", "tail", "--dir"])
        .arg(repo.root());
    let stdout = assert_finished("refs", &run_bounded(command, None));
    let refs: Vec<(u64, String)> = parse_lines(&stdout)[0]["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .map(|r| {
            (
                r["ln"].as_u64().unwrap_or_default(),
                r["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        refs,
        vec![(0, "def".to_string()), (3, "ref".to_string())],
        "定義と深い式の後ろの呼び出しだけ (closure の束縛は数えない)"
    );
}

/// dead-code の member liveness (同名メンバーの帰属) が、深いファイルで落ちない (F3-b / F3-c)。
/// 深いファイルの後ろの呼び出しからしか使われないメンバーを dead にしない。
#[test]
fn dead_code_member_liveness_handles_deep_files() {
    let deep_array = nested("[", "1", "]", 50_000);

    // TS: A.fmt と B.fmt の同名メンバー。B.fmt は深い配列の後ろからだけ呼ばれる。
    let ts = TestRepo::new();
    ts.write("a.ts", "export class A { fmt() { return \"a\"; } }\n");
    ts.write("b.ts", "export class B { fmt() { return \"b\"; } }\n");
    ts.write(
        "c.ts",
        "import { A } from \"./a\";\nexport const out = new A().fmt();\n",
    );
    ts.write(
        "deep.ts",
        format!(
            "import {{ B }} from \"./b\";\nconst D = {deep_array};\nexport function useD() {{ return [D, new B().fmt()]; }}\n"
        ),
    );

    // PHP: 同名の fmt の組があると、型の索引は無関係な深いファイルにも当たる。
    let php = TestRepo::new();
    php.write(
        "A.php",
        "<?php\nclass A { public function fmt() { return 1; } }\n",
    );
    php.write(
        "B.php",
        "<?php\nclass B { public function fmt() { return 2; } }\n",
    );
    php.write("c.php", "<?php\nfunction callA() { return A::fmt(); }\n");
    php.write(
        "deep.php",
        format!("<?php\n$d = {deep_array};\nfunction callB() {{ return B::fmt(); }}\n"),
    );

    for (label, repo, live) in [("ts", &ts, "B.fmt"), ("php", &php, "B.fmt")] {
        let mut command = cargo_bin();
        command.args(["dead-code", "--dir"]).arg(repo.root());
        let stdout = assert_finished(label, &run_bounded(command, None));
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("dead-code JSON");
        let dead: Vec<&str> = json["dead_symbols"]
            .as_array()
            .expect("dead_symbols")
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert!(
            !dead.contains(&live),
            "{label}: 深いファイルの後ろからだけ使う {live} は dead ではない: {dead:?}"
        );
    }
}

/// Angular の ControlValueAccessor 判定が、decorator の深い引数で落ちない。
/// Bash の trap の handler 文字列の深いサブシェルでも refs が落ちない。
#[test]
fn deep_decorator_and_trap_handler_do_not_crash() {
    let ng = TestRepo::new();
    ng.write(
        "c.ts",
        format!(
            "@Component({{ meta: {} }})\nexport class C {{\n  writeValue(v: unknown) {{ return v; }}\n}}\n",
            nested("[", "1", "]", 300_000)
        ),
    );
    ng.write(
        "use.ts",
        "import { C } from \"./c\";\nexport const c = new C();\n",
    );
    let mut command = cargo_bin();
    command.args(["dead-code", "--dir"]).arg(ng.root());
    assert_finished("angular dead-code", &run_bounded(command, None));

    let sh = TestRepo::new();
    sh.write(
        "a.sh",
        format!(
            "cleanup() {{ :; }}\ntrap '{}' INT\nafter() {{ cleanup; }}\n",
            nested("( ", "cleanup", " )", 50_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .args(["refs", "--name", "cleanup", "--dir"])
        .arg(sh.root());
    let stdout = assert_finished("trap refs", &run_bounded(command, None));
    let lines: Vec<u64> = parse_lines(&stdout)[0]["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .filter_map(|r| r["ln"].as_u64())
        .collect();
    assert_eq!(
        lines,
        vec![0, 1, 2],
        "定義・trap の handler・後ろの呼び出し"
    );
}

/// PHP の `$x->m()` の受け手の型推論が、関数本体の深い配列で落ちない (impact / context)。
#[test]
fn impact_handles_deep_php_function_body() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Repo.php",
        "<?php\nclass Repo {\n    public function store(array $row): bool { return true; }\n}\n",
    );
    repo.write(
        "Caller.php",
        format!(
            "<?php\nfunction persist(Repo $repo) {{\n    $d = {};\n    return $repo->store([]);\n}}\n",
            nested("[", "1", "]", 50_000)
        ),
    );
    repo.commit_all("init");
    repo.write(
        "Repo.php",
        "<?php\nclass Repo {\n    public function store(array $row, bool $flush): bool { return true; }\n}\n",
    );
    let mut command = cargo_bin();
    command.args(["context", "--git", "--dir"]).arg(repo.root());
    let stdout = assert_finished("context", &run_bounded(command, None));
    assert!(
        stdout.contains("Caller.php"),
        "深い本体の後ろの呼び出し元を返す: {}",
        stdout.chars().take(600).collect::<String>()
    );
}

/// `src/a.rs` の `f` を `a_old` から `a_new` へ変えた Cargo プロジェクト (`src/c.rs` が呼ぶ)。
fn rust_impact_repo(a_old: &str, a_new: &str, c: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod a;\npub mod c;\n");
    repo.write("src/a.rs", a_old);
    repo.write("src/c.rs", c);
    repo.commit_all("init");
    repo.write("src/a.rs", a_new);
    repo
}

/// `context --git` の `src/a.rs` の変更について、呼び出し元の (path, line) を種類ごとに返す。
fn rust_context_callers(label: &str, repo: &TestRepo) -> Vec<(String, Vec<(String, u64)>)> {
    let mut command = cargo_bin();
    command.args(["context", "--git", "--dir"]).arg(repo.root());
    let stdout = assert_finished(label, &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("context JSON");
    let change = json["changes"]
        .as_array()
        .and_then(|changes| changes.iter().find(|change| change["path"] == "src/a.rs"))
        .unwrap_or_else(|| panic!("{label}: {}", stdout.chars().take(400).collect::<String>()));
    [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ]
    .into_iter()
    .filter_map(|key| {
        let callers: Vec<(String, u64)> = change[key]
            .as_array()?
            .iter()
            .map(|caller| {
                (
                    caller["path"].as_str().unwrap_or_default().to_owned(),
                    caller["line"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        Some((key.to_owned(), callers))
    })
    .collect()
}

fn callers(entries: &[(&str, &[(&str, u64)])]) -> Vec<(String, Vec<(String, u64)>)> {
    entries
        .iter()
        .map(|(key, list)| {
            let list = list
                .iter()
                .map(|(path, line)| ((*path).to_owned(), *line))
                .collect();
            ((*key).to_owned(), list)
        })
        .collect()
}

/// Rust の証明 (型別名の同値・修飾パスの所有元・use ツリーの展開・std 起点の束縛・
/// 旧側の use の束縛名) は、深い入れ子でもスタックを溢れさせずに呼び出し元を返す。
/// どの深さも修正前は exit 134 (stack overflow) で落ちていた。
#[test]
fn rust_impact_proofs_handle_deep_nesting() {
    // 型別名の同値証明: 深い型を持つ関数ヘッダの正規化。
    let deep_type = nested("Option<", "u8", ">", 20_000);
    let repo = rust_impact_repo(
        "pub fn f(x: u32) -> u32 {\n    x\n}\n",
        &format!("pub type A = u32;\npub fn f(x: A, y: {deep_type}) -> u32 {{\n    x\n}}\n"),
        "pub fn g() -> u32 {\n    crate::a::f(1)\n}\n",
    );
    assert_eq!(
        rust_context_callers("alias context", &repo),
        callers(&[("impacted_callers", &[("src/c.rs", 1)])])
    );
    let mut command = cargo_bin();
    command.args(["impact", "--git", "--dir"]).arg(repo.root());
    let output = run_bounded(command, None);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "alias impact: {stderr}");
    assert!(
        stderr.contains("→ src/c.rs:2 [f]"),
        "alias impact: {stderr}"
    );

    // 修飾パスの分解: `crate::m::…::f(2)` の path を 1 段ずつたどる。
    let changed_f = "pub fn f(x: u32, y: u32) -> u32 {\n    x + y\n}\n";
    let repo = rust_impact_repo(
        "pub fn f(x: u32) -> u32 {\n    x\n}\n",
        changed_f,
        &format!(
            "pub fn g() -> u32 {{\n    crate::a::f(1)\n}}\npub fn h() -> u32 {{\n    crate::{}f(2)\n}}\n",
            "m::".repeat(10_000)
        ),
    );
    assert_eq!(
        rust_context_callers("path context", &repo),
        callers(&[("impacted_callers", &[("src/c.rs", 1), ("src/c.rs", 4)])])
    );

    // 再公開の所有元: 呼び出し元の import 行の use ツリーを展開する。
    let deep_use = |root: &str, depth: usize| {
        format!("use {root}::{{{};\n", nested("m::{", "x", "}", depth) + "}")
    };
    let repo = rust_impact_repo(
        "pub fn f(x: u32) -> u32 {\n    x\n}\n",
        changed_f,
        &format!(
            "use crate::{{a::f, {}}};\npub fn g() -> u32 {{\n    f(1)\n}}\n",
            nested("m::{", "x", "}", 3_000)
        ),
    );
    assert_eq!(
        rust_context_callers("reexport context", &repo),
        callers(&[
            ("impacted_callers", &[("src/c.rs", 2)]),
            ("informational_callers", &[("src/c.rs", 0)]),
        ])
    );

    // std 起点の証明: 参照ファイルの use ツリーから束縛を集める。
    let repo = rust_impact_repo(
        "pub fn f(x: u32) -> u32 {\n    x\n}\n",
        changed_f,
        &format!(
            "{}use crate::a::f;\npub fn g() -> u32 {{\n    f(1)\n}}\n",
            deep_use("std", 3_000)
        ),
    );
    assert_eq!(
        rust_context_callers("std origin context", &repo),
        callers(&[
            ("impacted_callers", &[("src/c.rs", 3)]),
            ("informational_callers", &[("src/c.rs", 1)]),
        ])
    );

    // 変更ファイルの旧側の use が束縛する名前と import の正規化。
    let old_use = deep_use("crate", 30_000);
    let repo = rust_impact_repo(
        &format!("{old_use}pub fn f(x: u32) -> u32 {{\n    x\n}}\n"),
        &format!("{old_use}{changed_f}"),
        "pub fn g() -> u32 {\n    crate::a::f(1)\n}\n",
    );
    assert_eq!(
        rust_context_callers("old use context", &repo),
        callers(&[("impacted_callers", &[("src/c.rs", 1)])])
    );
}

/// session は深い入力の要求の後も、同じ接続で後続の要求を処理する。
#[test]
fn session_keeps_serving_after_deep_inputs() {
    let repo = TestRepo::new();
    write_deep_functions(&repo);
    repo.write(
        "deep.h",
        format!(
            "// see foo::bar\nint deep_value = {};\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let input = [
        r#"{"command":"symbols","path":"ok.js"}"#,
        r#"{"command":"symbols","path":"fn_parens.js"}"#,
        r#"{"command":"symbols","path":"elif.js"}"#,
        r#"{"command":"refs","name":"deep_value","dir":"."}"#,
        r#"{"command":"symbols","path":"ok.js"}"#,
    ]
    .join("\n")
        + "\n";
    let mut command = cargo_bin();
    command
        .arg("session")
        .env("ASTRO_SIGHT_WORKSPACE", repo.root());
    let stdout = assert_finished("session", &run_bounded(command, Some(input)));
    let responses = parse_lines(&stdout);
    assert_eq!(responses.len(), 5, "5 要求すべてに応答する: {stdout}");
    for response in &responses {
        assert!(response.get("error").is_none(), "{response}");
    }
    assert!(
        responses[3]["refs"]
            .as_array()
            .is_some_and(|r| !r.is_empty())
    );
    assert_eq!(responses[4]["symbols"][0]["name"], "okay");
}

/// MCP サーバーは深い入力のツール呼び出しの後も、同じ接続で後続の要求に応答する。
/// ツールは tokio のワーカースレッドで走るので、そこでも落ちないことを確かめる。
#[test]
fn mcp_keeps_serving_after_deep_inputs() {
    let repo = TestRepo::new();
    write_deep_functions(&repo);
    repo.write(
        "deep.h",
        format!(
            "// see foo::bar\nint deep_value = {};\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let call = |id: u64, name: &str, arguments: serde_json::Value| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        })
        .to_string()
    };
    let requests = [
        call(
            2,
            "symbols_extract",
            serde_json::json!({ "path": "fn_parens.js" }),
        ),
        call(
            3,
            "symbols_extract",
            serde_json::json!({ "path": "elif.js" }),
        ),
        call(
            4,
            "refs_search",
            serde_json::json!({ "name": "deep_value", "dir": "." }),
        ),
        call(5, "symbols_extract", serde_json::json!({ "path": "ok.js" })),
    ];
    let stdout = mcp_requests_waiting_for_responses(repo.root(), &requests, &[2, 3, 4, 5]);
    for id in 2..=5 {
        let line = stdout
            .lines()
            .find(|line| line.contains(&format!("\"id\":{id}")))
            .unwrap_or_else(|| panic!("id={id} の応答が必要: {stdout}"));
        let json: serde_json::Value = serde_json::from_str(line).expect("valid JSON-RPC");
        assert!(json.get("result").is_some(), "id={id}: {json}");
        assert_ne!(json["result"]["isError"], true, "id={id}: {json}");
    }
}

/// API 差分と dead-code の値バインディングの signature 作成 (関数値の本体を省く処理) が、
/// 括弧が深く入れ子になった初期化子 (`export const DEEP = ((…1…));`) で落ちない。
/// 深い宣言の後ろの宣言 (`AFTER`) まで解析しきることも確かめる。
#[test]
fn api_signature_handles_deep_parenthesized_const() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("ok.ts", "export function okay() { return 1; }\n");
    repo.commit_all("init");
    repo.write(
        "deep.ts",
        format!(
            "export const DEEP = {};\nexport const AFTER = () => 1;\n",
            nested("(", "1", ")", 50_000)
        ),
    );

    let mut command = cargo_bin();
    command.args(["dead-code", "--dir"]).arg(repo.root());
    let stdout = assert_finished("dead-code", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("dead-code JSON");
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        dead.contains(&"AFTER"),
        "深い宣言の後ろの未使用 export も解析する: {dead:?}"
    );

    let mut command = cargo_bin();
    command.args(["review", "--git", "--dir"]).arg(repo.root());
    let stdout = assert_finished("review", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("review JSON");
    let added: Vec<&str> = json["api_changes"]["added"]
        .as_array()
        .expect("api_changes.added")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        added.contains(&"DEEP") && added.contains(&"AFTER"),
        "深い初期化子の宣言と後ろの宣言を API 差分に出す: {added:?}"
    );

    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    let output = run_bounded(command, None);
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "review --hook はシグナルで落ちずに判定を返す: {:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(400)
            .collect::<String>()
    );
}

#[test]
fn api_optional_object_extension_handles_deep_union() {
    let repo = TestRepo::new();
    repo.init_git();
    let branches = std::iter::repeat_n("'value'", 50_000)
        .collect::<Vec<_>>()
        .join(" | ");
    let source = |members: &str| {
        format!(
            "export function save(name: string, options: {{ {members} }} | {branches}): string {{ return name; }}\nexport function after(): number {{ return 1; }}\n"
        )
    };
    repo.write("api.ts", source("mode: string"));
    repo.write("use.ts", "import { save, after } from './api';\nexport const result = save('a', { mode: 'fast' }) + after();\n");
    repo.commit_all("initial");
    repo.write("api.ts", source("mode: string; label?: string"));

    let buckets = review_api_buckets("deep optional object union", &repo);
    assert!(
        in_bucket(&buckets, "compatible_modified", "save"),
        "{buckets:?}"
    );
    assert!(!in_bucket(&buckets, "modified", "save"), "{buckets:?}");
}

/// `review --git` を実行し、正常終了したことを確かめて API 差分の分類 → 名前の一覧を返す。
fn review_api_buckets(label: &str, repo: &TestRepo) -> serde_json::Map<String, serde_json::Value> {
    let mut command = cargo_bin();
    command.args(["review", "--git", "--dir"]).arg(repo.root());
    let stdout = assert_finished(label, &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("review JSON");
    json["api_changes"]
        .as_object()
        .expect("api_changes")
        .iter()
        .filter_map(|(bucket, items)| {
            let names: Vec<serde_json::Value> = items
                .as_array()?
                .iter()
                .filter_map(|item| item.get("name").cloned())
                .collect();
            (!names.is_empty()).then(|| (bucket.clone(), serde_json::Value::Array(names)))
        })
        .collect()
}

fn in_bucket(
    buckets: &serde_json::Map<String, serde_json::Value>,
    bucket: &str,
    name: &str,
) -> bool {
    buckets
        .get(bucket)
        .and_then(|names| names.as_array())
        .is_some_and(|names| names.iter().any(|n| n == name))
}

/// API 差分で削除した Python 関数の残存参照の帰属 (参照ファイルの別名と属性の受け手の
/// 収集) が、参照ファイルの深い式で落ちない。深い式の**後ろ**の受け手まで調べる:
/// 無関係なモジュールの `re.search` だけなら removed_dead、削除モジュールを受け手にした
/// `core.search` が残っていれば blocking な removed (対照)。
#[test]
fn api_removed_attribution_handles_deep_python_reference_file() {
    for (tail, bucket) in [
        (
            "import re\ndef run(s):\n    return re.search('a', s)\n",
            "removed_dead",
        ),
        (
            "from pkg import core\ndef run(s):\n    return core.search(s)\n",
            "removed",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("pkg");
        repo.write("pkg/__init__.py", "");
        repo.write("pkg/core.py", "def search(q):\n    return q\n");
        repo.write(
            "app.py",
            format!("DEEP = {}\n{tail}", nested("(", "1", ")", 50_000)),
        );
        repo.commit_all("init");
        repo.remove_file("pkg/core.py");
        let buckets = review_api_buckets(bucket, &repo);
        assert!(
            in_bucket(&buckets, bucket, "search"),
            "{bucket}: {buckets:?}"
        );
    }
}

/// export した object から未参照のメンバーを消したときの参照確認 (参照ファイル全体の
/// メンバー参照の走査) が、深い配列で落ちない。参照ファイルにキー名の文字列しか無ければ
/// 互換な変更、深い配列の後ろで `config.beta` を読んでいれば blocking な api.mod (対照)。
#[test]
fn api_object_member_check_handles_deep_reference_file() {
    for (tail, bucket) in [
        ("\"beta\"", "compatible_modified"),
        ("config.beta", "modified"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "config.ts",
            "export const config = { alpha: 1, beta: 2 };\n",
        );
        repo.write(
            "use.ts",
            format!(
                "import {{ config }} from './config';\nconst D = {};\nexport const a = [D, config.alpha, {tail}];\n",
                nested("[", "1", "]", 50_000)
            ),
        );
        repo.commit_all("init");
        repo.write("config.ts", "export const config = { alpha: 1 };\n");
        let buckets = review_api_buckets(bucket, &repo);
        assert!(
            in_bucket(&buckets, bucket, "config"),
            "{bucket}: {buckets:?}"
        );
    }
}

/// Rust の公開面の判定 (`pub use` の収集と module の走査) が、crate root の深い式で落ちない。
/// 深い式の後ろの `pub use api::found;` で再公開した関数の削除は blocking な removed、
/// 再公開していなければ private module の中の削除なので API 差分に出さない (対照)。
#[test]
fn api_rust_reexport_graph_handles_deep_module_file() {
    for (uses, removed) in [
        ("pub use api::found;\npub use api::kept;\n", true),
        ("pub use api::kept;\n", false),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write(
            "src/lib.rs",
            format!(
                "mod api;\npub const DEEP: u32 = {};\n{uses}",
                nested("(", "1", ")", 50_000)
            ),
        );
        repo.write("src/api.rs", "pub fn found() {}\npub fn kept() {}\n");
        repo.commit_all("init");
        repo.write("src/api.rs", "pub fn kept() {}\n");
        let buckets = review_api_buckets("rust", &repo);
        assert_eq!(
            in_bucket(&buckets, "removed", "found"),
            removed,
            "{buckets:?}"
        );

        let mut command = cargo_bin();
        command
            .args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root());
        let output = run_bounded(command, None);
        assert_eq!(
            output.status.code(),
            Some(i32::from(removed)),
            "review --hook はシグナルで落ちずに判定を返す: {:?}",
            output.status
        );
    }
}

/// 同一 diff で追随済みかの判定で、別ファイルの同名ローカル関数を見分ける処理 (字句スコープの
/// 束縛の探索) が、深い初期化子で落ちない。深い初期化子の後ろの `function startRecording` に
/// 束縛される呼び出しは対象外なので追随済み、`const` の束縛なら証明できず blocking (対照)。
#[test]
fn api_closure_shadow_resolution_handles_deep_initializer() {
    for (local, bucket) in [
        (
            "function startRecording() { return D; }",
            "modified_closed_in_diff",
        ),
        ("const startRecording = () => D;", "modified"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "lib.ts",
            "export function startRecording(a: number) { return a; }\n",
        );
        repo.write(
            "other.ts",
            format!(
                "const D = {};\n{local}\nexport function other() {{ return startRecording(); }}\n",
                nested("(", "1", ")", 50_000)
            ),
        );
        repo.write(
            "caller.ts",
            "import { startRecording } from './lib';\nexport function run() {\n  return startRecording(1);\n}\n",
        );
        repo.commit_all("init");
        repo.write(
            "lib.ts",
            "export function startRecording(a: number, b: number) { return a + b; }\n",
        );
        repo.write(
            "caller.ts",
            "import { startRecording } from './lib';\nexport function run() {\n  return startRecording(1, 2);\n}\n",
        );
        let buckets = review_api_buckets(bucket, &repo);
        assert!(
            in_bucket(&buckets, bucket, "startRecording"),
            "{bucket}: {buckets:?}"
        );
    }
}

/// bash の残存参照の帰属 (`source` するファイルのリテラル引数の収集) が、深いサブシェルで
/// 落ちない。深いサブシェルの後ろで削除元を `source` していれば同名の自前定義があっても
/// blocking な removed、`source` していなければ自前定義への参照なので removed_dead (対照)。
#[test]
fn api_bash_source_candidates_handle_deep_subshells() {
    for (tail, bucket) in [("source ./lib.sh\n", "removed"), ("", "removed_dead")] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("lib.sh", "helper() { echo hi; }\nother() { :; }\n");
        repo.write(
            "run.sh",
            format!(
                "#!/bin/bash\nhelper() {{ echo local; }}\nX=$({})\n{tail}helper\n",
                nested("( ", "echo", " )", 50_000)
            ),
        );
        repo.commit_all("init");
        repo.write("lib.sh", "other() { :; }\n");
        let buckets = review_api_buckets(bucket, &repo);
        assert!(
            in_bucket(&buckets, bucket, "helper"),
            "{bucket}: {buckets:?}"
        );
    }
}

fn names_in(json: &serde_json::Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = json[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} の配列が必要"))
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

/// symbols: 深い分割代入パターン (`export const [[[…x]]] = v;`) を束縛ごとのシンボルにする処理が
/// 落ちず、深い宣言の後ろの関数まで抽出する。
#[test]
fn symbols_handles_deep_destructuring_pattern() {
    let repo = TestRepo::new();
    repo.write(
        "deep.ts",
        format!(
            "export const {} = v;\nexport function after() {{ return 1; }}\n",
            nested("[", "x", "]", 50_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .current_dir(repo.root())
        .args(["symbols", "--no-cache", "--path", "deep.ts"]);
    let stdout = assert_finished("symbols", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("symbols JSON");
    assert_eq!(names_in(&json, "symbols"), ["after", "x"]);
}

/// refs: Rust の closure 引数のパターンが深く入れ子になっていても (`|((((y,),),),)| x`)、
/// 検索名を束縛しているかの判定が落ちない。束縛していない名前 (`x`) は closure の本体でも
/// 参照として数え、深い closure の後ろの呼び出しも数える。
#[test]
fn refs_handles_deep_closure_parameter_pattern() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        format!(
            "pub fn x() -> u32 {{ 1 }}\npub fn caller() -> u32 {{\n    let f = |{}| x;\n    let _ = f;\n    x()\n}}\n",
            nested("(", "y", ",)", 50_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .args(["refs", "--name", "x", "--dir"])
        .arg(repo.root());
    let stdout = assert_finished("refs", &run_bounded(command, None));
    let refs: Vec<(u64, String)> = parse_lines(&stdout)[0]["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .map(|r| {
            (
                r["ln"].as_u64().unwrap_or_default(),
                r["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        refs,
        vec![
            (0, "def".to_string()),
            (2, "ref".to_string()),
            (4, "ref".to_string())
        ]
    );
}

/// lint: 深い式を持つファイルでもテキストパターンの照合が落ちず、深い式の中と後ろの
/// 両方の一致を返す。
#[test]
fn lint_handles_deep_expressions() {
    let repo = TestRepo::new();
    repo.write(
        "deep.js",
        format!(
            "export const v = {};\nexport const after = needle_after;\n",
            nested("(", "needle", ")", 50_000)
        ),
    );
    repo.write(
        "rules.yaml",
        "- id: needle\n  language: javascript\n  pattern: needle\n  severity: warning\n  message: found\n",
    );
    let mut command = cargo_bin();
    command
        .current_dir(repo.root())
        .args(["lint", "--path", "deep.js", "--rules", "rules.yaml"]);
    let stdout = assert_finished("lint", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("lint JSON");
    let matches: Vec<(u64, &str)> = json["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|m| {
            (
                m["line"].as_u64().unwrap_or_default(),
                m["matched_text"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(matches, [(0, "needle"), (1, "needle_after")]);
}

/// calls: 宣言子が深く入れ子になった C の関数 (`int ****…f(void)`) でも、呼び出し元の名前の
/// 取り出しが落ちない。深い関数と後ろの関数の呼び出しを両方返す。
#[test]
fn calls_handles_deep_pointer_declarator() {
    let repo = TestRepo::new();
    repo.write(
        "deep.c",
        format!(
            "int g(void) {{ return 1; }}\nint {}f(void) {{ return g(); }}\nint after(void) {{ return g(); }}\n",
            "*".repeat(50_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .current_dir(repo.root())
        .args(["calls", "--path", "deep.c"]);
    let stdout = assert_finished("calls", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("calls JSON");
    let calls: Vec<(String, Vec<String>)> = json["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|c| {
            (
                c["caller"].as_str().unwrap_or_default().to_string(),
                c["callees"]
                    .as_array()
                    .expect("callees")
                    .iter()
                    .filter_map(|callee| callee["name"].as_str().map(str::to_string))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("f".to_string(), vec!["g".to_string()]),
            ("after".to_string(), vec!["g".to_string()])
        ]
    );
}

/// dead-code: 深い入力で、Java の長い修飾名の照合 (Flyway の基底クラス判定)、PHP の深い
/// 制御構造の中の `return new self('X')` (疑似 enum の判定)、JS/TS の引数の深い分割代入
/// (同名メンバーの帰属で束縛を集める処理)、`**` が長く続く `.gitattributes` の照合が落ちず、
/// それぞれの結論 (疑似 enum の値は dead にしない・帰属できた `Alpha.fmt` は live・
/// 生成物は候補から外す) を変えない。
#[test]
fn dead_code_handles_deep_names_conditions_patterns_and_gitattributes() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/Deep.java",
        format!(
            "public class Deep extends {}Base {{ }}\n",
            "a.".repeat(50_000)
        ),
    );
    repo.write(
        "src/Status.php",
        format!(
            "<?php\nfinal class Status {{\n    public static function ACTIVE(): self {{\n        {}return new self('ACTIVE');{}\n    }}\n}}\n",
            "if (true) { ".repeat(20_000),
            " }".repeat(20_000)
        ),
    );
    repo.write(
        "src/alpha.ts",
        "export class Alpha {\n  fmt(): number { return 1; }\n}\n",
    );
    repo.write(
        "src/beta.ts",
        "export class Beta {\n  fmt(): number { return 2; }\n}\n",
    );
    repo.write(
        "src/use.ts",
        format!(
            "import {{ Alpha }} from \"./alpha\";\nfunction g({}) {{ return deep; }}\nexport const out = new Alpha().fmt() + g([]);\n",
            nested("[", "deep", "]", 200_000)
        ),
    );
    repo.write(
        ".gitattributes",
        format!("{}gen.ts linguist-generated\n", "**/".repeat(200_000)),
    );
    repo.write(
        "src/gen.ts",
        "export function generatedHelper() { return 1; }\n",
    );

    let mut command = cargo_bin();
    command.args(["dead-code", "--dir"]).arg(repo.root());
    let stdout = assert_finished("dead-code", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("dead-code JSON");
    assert_eq!(
        names_in(&json, "dead_symbols"),
        ["Beta", "Beta.fmt", "Deep", "Status", "out"]
    );
    assert_eq!(
        json["generated_candidates_skipped"]["paths"],
        serde_json::json!(["src/gen.ts"])
    );
}

/// calls: 入れ子の呼び出し (`wrap(wrap(…(() => inner())…))`) の呼び出し元の解決が、深さの
/// 2 乗以上の時間にならない。旧実装は呼び出しごとに `Node::parent` (根から降りる) で祖先を
/// たどっていたため、release でも 1,000 段で約 30 秒、2,000 段で 2 分を超えた。トップレベルの
/// 2 万段と関数の中の 5,000 段を期限内に解析し、関数の中の呼び出しは匿名 callback の中も含めて
/// すべてその関数を呼び出し元にする (トップレベルの呼び出しは呼び出し元が無いので出さない)。
///
/// 2 万段にとどめるのは、tree-sitter のクエリ自体が約 3.2 万段より深いノードの一致を返さず
/// (5 万段で 32,767 件)、その付近から急に遅くなるため (この関数の外の制約)。
#[test]
fn calls_resolves_callers_of_deeply_nested_calls_in_time() {
    let repo = TestRepo::new();
    repo.write(
        "deep.ts",
        format!(
            "export const top = {};\nexport function outer() {{ return {}; }}\nexport function after() {{ return g(); }}\n",
            nested("wrap(", "() => 1", ")", 20_000),
            nested("wrap(", "() => inner()", ")", 5_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .current_dir(repo.root())
        .args(["calls", "--path", "deep.ts"]);
    let stdout = assert_finished("calls", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("calls JSON");
    let groups: Vec<(String, usize, usize)> = json["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|group| {
            let callees = group["callees"].as_array().expect("callees");
            let count = |name: &str| callees.iter().filter(|c| c["name"] == name).count();
            (
                group["caller"].as_str().unwrap_or_default().to_string(),
                count("wrap"),
                callees.len() - count("wrap"),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![("outer".to_string(), 5_000, 1), ("after".to_string(), 0, 1)]
    );
}
