//! refs サブコマンド (参照検索) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn refs_finds_symbol() {
    let output = cargo_bin()
        .args(["refs", "--name", "AstgenResponse", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("version").is_none(),
        "refs should not have version"
    );
    assert_eq!(json["symbol"], "AstgenResponse");

    let refs = json["refs"].as_array().unwrap();
    assert!(
        refs.len() >= 2,
        "Should find AstgenResponse in multiple files"
    );

    // Should have at least one definition
    let defs: Vec<_> = refs.iter().filter(|r| r["kind"] == "def").collect();
    assert!(!defs.is_empty(), "Should find definition of AstgenResponse");
}

#[test]
fn refs_with_glob_filter() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "AstgenResponse",
            "--dir",
            "src/",
            "--glob",
            "**/*.rs",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    assert!(!refs.is_empty());
}

// ---- Refs --names batch tests ----

#[test]
fn refs_batch_names() {
    let output = cargo_bin()
        .args([
            "refs",
            "--names",
            "AppService,AstgenResponse",
            "--dir",
            "src/",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Should have 2 NDJSON lines for 2 symbols");

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["symbol"], "AppService");
    assert!(!first["refs"].as_array().unwrap().is_empty());

    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["symbol"], "AstgenResponse");
    assert!(!second["refs"].as_array().unwrap().is_empty());
}

/// chunk サイズより多い名前でも、複数 chunk にまたがって入力 names 順の
/// NDJSON 出力を維持することを検証（cmd_refs_batch の chunk 化回帰: 2026-05-31）。
#[test]
fn refs_batch_names_preserves_order_across_chunks() {
    // ASTRO_SIGHT_REFS_BATCH_CHUNK=2 で 3 名前を chunk [A,B] / [C] に分割させ、
    // chunk 境界をまたいでも入力順を保つことを確認する。
    let output = cargo_bin()
        .env("ASTRO_SIGHT_REFS_BATCH_CHUNK", "2")
        .args([
            "refs",
            "--names",
            "AppService,AstgenResponse,SymbolReference",
            "--dir",
            "src/",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let symbols: Vec<String> = stdout
        .trim()
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["symbol"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        symbols,
        vec!["AppService", "AstgenResponse", "SymbolReference"],
        "chunk をまたいでも入力 names 順を維持すべき"
    );
}

/// find_references_batch はディレクトリ走査を 1 回に集約しつつ内部で名前を chunk
/// 分割するため、chunk サイズを変えても参照集合が完全に一致しなければならない。
/// 走査集約リファクタが結果を変えていないことを保証する回帰テスト。
#[test]
fn refs_batch_results_independent_of_chunk_size() {
    let names =
        "find_references_batch,collect_files,extract_symbols,detect_api_changes,SymbolReference";
    let run = |chunk: &str| -> String {
        let output = cargo_bin()
            .env("ASTRO_SIGHT_REFS_BATCH_CHUNK", chunk)
            .args(["refs", "--names", names, "--dir", "src/"])
            .output()
            .expect("failed to run");
        assert!(output.status.success());
        // (symbol, 参照件数) を symbol 順に正規化する。chunk 分割で参照が
        // 取りこぼされたり重複したりすれば件数が変わって検出できる。
        let mut pairs: Vec<(String, usize)> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                let sym = v["symbol"].as_str().unwrap().to_string();
                let n = v["refs"].as_array().map(|a| a.len()).unwrap_or(0);
                (sym, n)
            })
            .collect();
        pairs.sort();
        format!("{pairs:?}")
    };
    let chunk_big = run("64");
    assert_eq!(
        run("1"),
        chunk_big,
        "chunk=1 と chunk=64 で結果が一致すべき"
    );
    assert_eq!(
        run("2"),
        chunk_big,
        "chunk=2 と chunk=64 で結果が一致すべき"
    );
    // 全件 0 だと比較が無意味になるため、参照が実際に検出されていることも確認する。
    assert!(
        chunk_big.contains("detect_api_changes"),
        "参照が検出されているべき: {chunk_big}"
    );
}

#[test]
fn refs_name_or_names_required() {
    let output = cargo_bin()
        .args(["refs", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());
}

#[test]
fn refs_rejects_empty_name() {
    let output = cargo_bin()
        .args(["refs", "--name", "", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--name")
    );
}

// ---- refs 異常系テスト ----

#[test]
fn refs_nonexistent_directory() {
    let output = cargo_bin()
        .args(["refs", "--name", "foo", "--dir", "/nonexistent/dir/path"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "FILE_NOT_FOUND");
}

#[test]
fn refs_rejects_file_as_dir_argument() {
    let output = cargo_bin()
        .args(["refs", "--name", "main", "--dir", "src/main.rs"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a directory")
    );
}

#[test]
fn refs_whitespace_only_name() {
    // 空白のみの name は拒否される（trim 後に空になる）
    let output = cargo_bin()
        .args(["refs", "--name", "   ", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
}

// ---- refs --names 境界値テスト ----

#[test]
fn refs_batch_names_empty_after_trim() {
    // 空白のみの names は拒否される
    let output = cargo_bin()
        .args(["refs", "--names", " , , ", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
}

/// 拡張子なし shebang スクリプト (例: `bin/install`) が collect_files の対象として
/// 拾われ、refs / dead-code 検索に含まれることを検証
/// (Issue: shebang-script-collect-files)。
#[test]
fn refs_picks_up_shebang_script_without_extension() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("bin")).unwrap();

    // 拡張子なし bash スクリプト (shebang 付き)
    std::fs::write(
        root.join("bin/install"),
        "#!/usr/bin/env bash\nfoo() { echo hi; }\nfoo\n",
    )
    .unwrap();
    // 同じ名前を呼ぶ普通の .sh
    std::fs::write(root.join("deploy.sh"), "#!/usr/bin/env bash\nfoo\n").unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "foo", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let paths: Vec<&str> = refs.iter().filter_map(|r| r["path"].as_str()).collect();
    assert!(
        paths.iter().any(|p| p.ends_with("bin/install")),
        "拡張子なし shebang スクリプト bin/install が refs 対象に入るべき: {paths:?}"
    );
}

#[test]
fn refs_php_callable_array_method_is_detected() {
    // N3: `[Class::class, 'method']` の string literal を method ref として扱う。
    // tree-sitter の identifier ノードには現れない (string 内) ため、
    // AST レベルで `array_creation_expression` を special-case 抽出する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Target {\n\
    public static function handler() { return 1; }\n\
}\n\
class Caller {\n\
    public function dispatch() {\n\
        $x = [Target::class, 'handler'];\n\
        return call_user_func($x);\n\
    }\n\
}\n",
    )
    .unwrap();

    // refs --name (single)
    let output = cargo_bin()
        .args(["refs", "--name", "handler", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let has_callable_ref = refs.iter().any(|r| {
        r["kind"].as_str() == Some("ref")
            && r["ctx"]
                .as_str()
                .is_some_and(|c| c.contains("[Target::class, 'handler']"))
    });
    assert!(
        has_callable_ref,
        "callable array `[Target::class, 'handler']` の 'handler' を ref として検出するべき: {refs:?}"
    );

    // dead-code 経由でも同等に効くこと (refs スコープを通って Target も生存判定)
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead.iter().any(|n| n.contains("handler")),
        "callable array で参照される handler が dead に出てはならない: {dead:?}"
    );
}

#[test]
fn refs_php_string_callable_class_at_method_is_detected() {
    // N4: `'ClassName@method'` 形式の文字列 callable (Laravel 5.x 以前互換) を method ref として扱う。
    // tree-sitter は string 全体を 1 ノードとしてしか出さないため、内容を pattern match して
    // `@` 以降を method 名として抽出する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Target {\n\
    public function handler() { return 1; }\n\
}\n\
function register() {\n\
    return 'Target@handler';\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "handler", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let has_string_ref = refs.iter().any(|r| {
        r["kind"].as_str() == Some("ref")
            && r["ctx"]
                .as_str()
                .is_some_and(|c| c.contains("'Target@handler'"))
    });
    assert!(
        has_string_ref,
        "'Target@handler' の 'handler' を ref として検出すべき: {refs:?}"
    );
}

#[test]
fn refs_php_concat_class_class_at_method_is_detected() {
    // N4: `Class::class . '@method'` 形式の concat callable を method ref として扱う。
    // `'@method'` 単独の string は、親が `binary_expression` (`.` operator) かつ左辺が
    // `class_constant_access_expression` (`X::class`) の場合のみ ref 認定する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Target {\n\
    public function dispatch() { return 1; }\n\
}\n\
function register() {\n\
    return Target::class . '@dispatch';\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "dispatch",
            "--dir",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let has_concat_ref = refs.iter().any(|r| {
        r["kind"].as_str() == Some("ref")
            && r["ctx"]
                .as_str()
                .is_some_and(|c| c.contains("Target::class . '@dispatch'"))
    });
    assert!(
        has_concat_ref,
        "Target::class . '@dispatch' の 'dispatch' を ref として検出すべき: {refs:?}"
    );
}

#[test]
fn refs_php_email_string_does_not_produce_fake_ref() {
    // N4 誤検出防止: `'user@example.com'` のようなメール風文字列は method ref にしない。
    // class_part='user' は先頭小文字 → reject、method_part='example.com' は `.` 含む → reject。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
function example() { return 'example.com'; }\n\
function contact() { return 'user@example.com'; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "example", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let fake_refs: Vec<_> = refs
        .iter()
        .filter(|r| {
            r["kind"].as_str() == Some("ref")
                && r["ctx"]
                    .as_str()
                    .is_some_and(|c| c.contains("'user@example.com'"))
        })
        .collect();
    assert!(
        fake_refs.is_empty(),
        "メール風文字列を method ref にしてはならない: {fake_refs:?}"
    );
}

#[test]
fn refs_php_callable_array_rejects_non_class_const_first_element() {
    // N3 誤検出防止: 第1要素が `Class::class` でない場合は ref として認めない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Helper {\n\
    public static function doIt() { return 1; }\n\
}\n\
function f() { return [1, 'doIt']; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "doIt", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        ref_count, 0,
        "[1, 'doIt'] は callable array ではないので ref を作るべきでない: {refs:?}"
    );
}

/// GitLab issue #7 補助テスト: `refs --name` で PHP の `ClassName::method()` 形式の
/// cross-file 呼び出しが `kind: "ref"` として返ることを確認する。
/// dead-code の usage 集計はこの refs 経路 (count_non_definition_refs_split → count_refs_in_file)
/// と同じ AST 走査で行われるため、ここが ref として認識されることが
/// dead 誤検出回避の前提条件。
#[test]
fn refs_php_cross_file_scoped_static_call_is_detected_as_ref() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("AHelper.php"),
        "<?php\nclass AHelper {\n    public static function voFoo(): string { return 'foo'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("BConsumer.php"),
        "<?php\nclass BConsumer {\n    public function doSomething(): void { $x = AHelper::voFoo(); echo $x; }\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "voFoo", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs array");
    let cross_file_refs: Vec<&serde_json::Value> = refs
        .iter()
        .filter(|r| {
            r["kind"].as_str() == Some("ref")
                && r["path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with("BConsumer.php"))
        })
        .collect();
    assert_eq!(
        cross_file_refs.len(),
        1,
        "BConsumer.php からの `AHelper::voFoo()` は 1 件の ref として検出されるべき: refs={refs:?}"
    );
}

/// Python の型注釈位置は CLI 出力でも `ref` として現れる。
///
/// 戻り値型が `def` に分類されていたため、`api.add` に添える同一ファイル内実利用参照数
/// (`refs_internal`) が 0 になり「未参照の新規公開 API」と誤って警告されていた。
/// 宣言名は `def` のままであること (対照) も同時に確認する。
#[test]
fn refs_python_return_type_annotation_is_reference() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("mod.py"),
        "class Payload:\n\
    pass\n\
\n\
\n\
def build() -> Payload:\n\
    return Payload()\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "Payload", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();

    let defs: Vec<_> = refs.iter().filter(|r| r["kind"] == "def").collect();
    let uses: Vec<_> = refs.iter().filter(|r| r["kind"] == "ref").collect();
    assert_eq!(defs.len(), 1, "class 宣言だけが def: {refs:?}");
    assert_eq!(
        uses.len(),
        2,
        "戻り値型注釈と本体のコンストラクタ呼び出しが ref: {refs:?}"
    );
}
