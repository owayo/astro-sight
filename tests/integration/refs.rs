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

/// JS/TS の object literal shorthand (`{ handler }`) を参照として数え、
/// destructuring の shorthand (`const { picked } = ...`) は数えないことを固定する。
///
/// 旧実装は `shorthand_property_identifier` を identifier として扱わなかったため、
/// レジストリ / DI / `module.exports = { a, b }` のような JS/TS で最も一般的な参照形が
/// 1 件も数えられず、**使われている関数が dead-code に出ていた**。
/// 束縛側 (`shorthand_property_identifier_pattern`) を数えてしまう逆方向の事故を防ぐため、
/// 「shorthand が ref になる」ことと「束縛が ref にならない」ことを同じテストで固定する。
#[test]
fn refs_counts_object_shorthand_value_but_not_destructuring_binding() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("registry.ts"),
        "export function handler() { return 1; }\n\
         export const registry = { handler };\n\
         function source() { return { picked: 1 }; }\n\
         const { picked } = source();\n\
         export const out = picked;\n",
    )
    .expect("write fixture");
    let dir_arg = dir.path().to_str().expect("utf-8 path");

    let refs_of = |name: &str| -> Vec<serde_json::Value> {
        let output = cargo_bin()
            .args(["refs", "--name", name, "--dir", dir_arg])
            .output()
            .expect("failed to run");
        assert!(output.status.success());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        json["refs"].as_array().cloned().unwrap_or_default()
    };

    // `{ handler }` は値の読み出しなので ref として数える
    let handler = refs_of("handler");
    assert!(
        handler.iter().any(|r| r["kind"] == "ref" && r["ln"] == 1),
        "object shorthand `{{ handler }}` should be counted as a reference: {handler:?}"
    );

    // `const { picked } = ...` は束縛なので ref にしない (数えると dead-code が fail-open する)
    let picked = refs_of("picked");
    assert!(
        !picked.iter().any(|r| r["ln"] == 3),
        "destructuring binding should not be counted as a reference: {picked:?}"
    );

    // shorthand で参照されている関数が dead に出ないこと (本来の症状)
    let output = cargo_bin()
        .args(["dead-code", "--dir", dir_arg])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !dead.contains(&"handler"),
        "`handler` is referenced via object shorthand and must not be dead: {dead:?}"
    );
}

/// TS の computed member 名 `class A { [SOME_KEY]() {} }` は定数への**参照**であって
/// 定義ではないことを固定する。
///
/// 旧実装は「grandparent が定義ノードで、その name フィールドが parent と同一なら def」
/// という判定 (分割代入パターン向け) を持っており、method_definition の name フィールドが
/// `computed_property_name` になる TS でこの条件に該当してしまっていた。結果として
/// 同一ファイル内で使われている定数が dead-code に出ていた。
///
/// **対照ケースを内蔵する**: 通常のメソッド名 (`plain()`) は従来どおり def のまま。
/// これが無いと「computed を ref にする」修正が通常のメソッド定義まで ref に落とす
/// 逆方向の事故 (定義が 1 つも無くなる) を検出できない。
#[test]
fn refs_treats_computed_member_name_as_reference_not_definition() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("keys.ts"),
        "export const SOME_KEY = \"s\";\n\
         export class A {\n\
         \x20 [SOME_KEY]() { return 1; }\n\
         \x20 plain() { return 2; }\n\
         }\n",
    )
    .expect("write fixture");
    let dir_arg = dir.path().to_str().expect("utf-8 path");

    let refs_of = |name: &str| -> Vec<serde_json::Value> {
        let output = cargo_bin()
            .args(["refs", "--name", name, "--dir", dir_arg])
            .output()
            .expect("failed to run");
        assert!(output.status.success());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        json["refs"].as_array().cloned().unwrap_or_default()
    };

    let some_key = refs_of("SOME_KEY");
    let computed = some_key
        .iter()
        .find(|r| r["ln"] == 2)
        .unwrap_or_else(|| panic!("computed member name not found in refs: {some_key:?}"));
    assert_eq!(
        computed["kind"], "ref",
        "`[SOME_KEY]()` reads the constant, so it is a reference: {some_key:?}"
    );

    // 対照: 通常のメソッド名は定義のまま
    let plain = refs_of("plain");
    assert!(
        plain.iter().any(|r| r["kind"] == "def"),
        "a normal method name must stay a definition: {plain:?}"
    );

    let output = cargo_bin()
        .args(["dead-code", "--dir", dir_arg])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !dead.contains(&"SOME_KEY"),
        "`SOME_KEY` is used as a computed member name and must not be dead: {dead:?}"
    );
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

#[test]
fn refs_reports_generated_skips_and_supports_opt_in_and_explicit_glob() {
    let dir = tempfile::TempDir::new().unwrap();
    let generated = dir.path().join("generated.rb");
    std::fs::write(
        &generated,
        "# @generated by a tool\ndef generated_needle; end\ngenerated_needle\n",
    )
    .unwrap();

    let run = |extra: &[&str]| {
        let mut command = cargo_bin();
        command.args(["refs", "--name", "generated_needle", "--dir"]);
        command.arg(dir.path());
        command.args(extra);
        command.output().expect("run refs")
    };

    let default = run(&[]);
    assert!(default.status.success());
    let json: serde_json::Value = serde_json::from_slice(&default.stdout).unwrap();
    assert!(json["refs"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["generated"], 1);
    assert_eq!(json["skipped"]["paths"][0], "generated.rb");

    let included = run(&["--include-generated"]);
    assert!(included.status.success());
    let json: serde_json::Value = serde_json::from_slice(&included.stdout).unwrap();
    assert!(!json["refs"].as_array().unwrap().is_empty());
    assert!(json.get("skipped").is_none());

    let explicit = run(&["--glob", "**/generated.rb"]);
    assert!(explicit.status.success());
    let json: serde_json::Value = serde_json::from_slice(&explicit.stdout).unwrap();
    assert!(!json["refs"].as_array().unwrap().is_empty());
    assert!(json.get("skipped").is_none());
}

#[test]
fn refs_legacy_generated_env_optout_is_process_isolated() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("_ide_helper.php"),
        "<?php\nfunction legacy_env_needle() {}\nlegacy_env_needle();\n",
    )
    .unwrap();

    let mut default_command = cargo_bin();
    let default = default_command
        .args(["refs", "--name", "legacy_env_needle", "--dir"])
        .arg(dir.path())
        .args(["--glob", "**/*.php"])
        .env_remove("ASTRO_SIGHT_NO_GENERATED_EXCLUSION")
        .output()
        .expect("run refs with default generated exclusion");
    assert!(default.status.success());
    let json: serde_json::Value = serde_json::from_slice(&default.stdout).unwrap();
    assert!(json["refs"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["generated"], 1);

    let mut optout_command = cargo_bin();
    let optout = optout_command
        .args(["refs", "--name", "legacy_env_needle", "--dir"])
        .arg(dir.path())
        .args(["--glob", "**/*.php"])
        .env("ASTRO_SIGHT_NO_GENERATED_EXCLUSION", "1")
        .output()
        .expect("run refs with legacy generated exclusion opt-out");
    assert!(optout.status.success());
    let json: serde_json::Value = serde_json::from_slice(&optout.stdout).unwrap();
    assert!(!json["refs"].as_array().unwrap().is_empty());
    assert!(json.get("skipped").is_none());
}

#[test]
fn refs_generated_marker_rejects_prose_and_string_literals() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("prose.rb"),
        "# Detects double disable comments.\n# automatically generated comments must be regenerated.\ndef marker_needle; end\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("literal.rb"),
        "NOTICE = \"DO NOT EDIT\"\ndef marker_needle; end\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "marker_needle", "--dir"])
        .arg(dir.path())
        .output()
        .expect("run refs");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let paths: std::collections::HashSet<&str> = json["refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|reference| reference["path"].as_str())
        .collect();
    assert!(paths.contains("prose.rb"), "prose file was omitted: {json}");
    assert!(
        paths.contains("literal.rb"),
        "string-literal file was omitted: {json}"
    );
    assert!(json.get("skipped").is_none());
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

#[test]
fn refs_batch_reports_generated_skip_without_changing_record_count() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("generated.rb"),
        "# @generated\ndef hidden_name; end\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("real.rb"), "def visible_name; end\n").unwrap();

    let output = cargo_bin()
        .args(["refs", "--names", "hidden_name,visible_name", "--dir"])
        .arg(dir.path())
        .output()
        .expect("run batch refs");
    assert!(output.status.success());
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2, "one record per requested symbol");
    assert_eq!(records[0]["symbol"], "hidden_name");
    assert_eq!(records[0]["skipped"]["generated"], 1);
    assert_eq!(records[0]["skipped"]["paths"][0], "generated.rb");
    assert_eq!(records[1]["symbol"], "visible_name");
    assert!(records[1].get("skipped").is_none());
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

/// PHP で `refs --name` (single 経路) と `refs --names` (batch 経路) が
/// **同じ入力に対して同じ結果**を返すことを CLI レベルで固定する。
///
/// 旧実装は batch 側の name index が case-insensitive 用の折りたたみキー (小文字) と
/// 原文キーを同じ map に混在させていたため、case-sensitive 文脈 (`$this->foo` の
/// プロパティ名や `$foo` 変数) の lookup がシンボル `Foo` の折りたたみキーへ誤ヒットした。
/// `refs --names` が使う `find_references_batch` は `ApiRefIndex` の土台なので、
/// この汚染は api.rm / api.mod / refs_internal / dead-code まで波及する。
#[test]
fn refs_single_and_batch_agree_on_php_case_domains() {
    let repo = TestRepo::new();
    repo.write(
        "app.php",
        "<?php\n\
class Foo {\n\
    public $foo = 1;\n\
    public function bar() { return $this->foo; }\n\
}\n\
function foo(): int { return 2; }\n\
$Foo = new Foo();\n\
$foo = FOO::bar();\n\
echo foo();\n",
    );

    // batch 経路: NDJSON (1 行 = 1 シンボル)
    let out = cargo_bin()
        .args(["refs", "--names", "Foo,foo", "--dir"])
        .arg(repo.root())
        .output()
        .expect("failed to run refs --names");
    assert!(out.status.success(), "refs --names failed");
    let batch: std::collections::HashMap<String, serde_json::Value> =
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).expect("invalid NDJSON");
                (v["symbol"].as_str().unwrap().to_string(), v)
            })
            .collect();

    // 位置と分類まで含めた指紋で比較する (件数一致だけでは取りこぼす)。
    let fingerprint = |v: &serde_json::Value| -> Vec<(u64, u64, String)> {
        let mut out: Vec<_> = v["refs"]
            .as_array()
            .expect("refs array")
            .iter()
            .map(|r| {
                (
                    r["ln"].as_u64().unwrap(),
                    r["col"].as_u64().unwrap(),
                    r["kind"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        out.sort();
        out
    };

    for name in ["Foo", "foo"] {
        let single_out = cargo_bin()
            .args(["refs", "--name", name, "--dir"])
            .arg(repo.root())
            .output()
            .expect("failed to run refs --name");
        assert!(single_out.status.success(), "refs --name {name} failed");
        let single: serde_json::Value =
            serde_json::from_slice(&single_out.stdout).expect("invalid JSON");
        assert_eq!(
            fingerprint(&single),
            fingerprint(&batch[name]),
            "single と batch は同一入力に対して同一結果を返すこと (name={name})"
        );
    }

    // 症状の直接固定: case-sensitive なプロパティ `$this->foo` (0-origin 行 3) を
    // `Foo` の参照に数えない。
    let foo_class = fingerprint(&batch["Foo"]);
    assert!(
        !foo_class.iter().any(|(ln, _, _)| *ln == 3),
        "case-sensitive なプロパティ参照を class `Foo` に帰属させないこと: {foo_class:?}"
    );
    // 対照: case-insensitive な `FOO::bar()` (0-origin 行 7) は `Foo` の参照として残す。
    assert!(
        foo_class.iter().any(|(ln, _, _)| *ln == 7),
        "case-insensitive なクラス参照は残すこと (抑制しすぎない): {foo_class:?}"
    );
}

/// 出力件数の上限が既定で効き、省略が起きたときだけ `result_summary` を出す。
///
/// 「上限に当たらない問い合わせでは出力が従来とバイト単位で同一」という契約を、
/// **同じテストで対照として固定する** — 片方だけだと「常にサマリを出す」退行を
/// 検出できない (既存 consumer の JSON パースが壊れる方向の事故)。
#[test]
fn refs_applies_default_result_limits_and_declares_omissions() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 上限 (既定 100) を確実に超える件数の参照を作る。
    let mut src = String::from("pub fn hotsym() {}\n");
    for i in 0..300 {
        src.push_str(&format!("pub fn caller{i}() {{ hotsym(); }}\n"));
    }
    std::fs::write(dir.path().join("a.rs"), &src).expect("write");

    let output = cargo_bin()
        .args(["refs", "--name", "hotsym", "--dir"])
        .arg(dir.path())
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    let shown = json["refs"].as_array().expect("refs").len();
    let summary = &json["result_summary"];
    assert!(!summary.is_null(), "省略が起きたら申告する: {json}");
    assert_eq!(summary["shown"].as_u64().expect("shown") as usize, shown);
    // 解析は止めないので total は正確 (定義 1 + 呼び出し 300)
    assert_eq!(summary["total"], 301);
    assert_eq!(
        summary["omitted"].as_u64().expect("omitted") as usize,
        301 - shown
    );
    assert!(shown < 301, "上限が効いていない: shown={shown}");
    assert_eq!(summary["complete_input"], true);
    assert!(
        summary["limited_by"]
            .as_array()
            .expect("limited_by")
            .iter()
            .any(|v| v == "max_results" || v == "token_budget"),
        "どの上限で切れたかを申告する: {summary}"
    );
    // 省略分の内訳は「省略された分だけ」を数える
    let by_kind_ref = summary["by_kind"]["ref"].as_u64().expect("by_kind.ref");
    assert_eq!(by_kind_ref as usize, 301 - shown);
    assert_eq!(summary["by_lang"]["rust"], by_kind_ref);

    // 対照: 上限に当たらない問い合わせでは result_summary を出さない
    let output = cargo_bin()
        .args(["refs", "--name", "caller7", "--dir"])
        .arg(dir.path())
        .output()
        .expect("failed to run");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("result_summary").is_none(),
        "上限に当たらなければ出力は従来と同一: {json}"
    );
}

/// `unlimited` で全件返り、サマリも出ない (明示的な全件取得の導線)。
#[test]
fn refs_unlimited_returns_every_reference() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut src = String::from("pub fn hotsym() {}\n");
    for i in 0..300 {
        src.push_str(&format!("pub fn caller{i}() {{ hotsym(); }}\n"));
    }
    std::fs::write(dir.path().join("a.rs"), &src).expect("write");

    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "hotsym",
            "--max-results",
            "unlimited",
            "--token-budget",
            "unlimited",
            "--dir",
        ])
        .arg(dir.path())
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["refs"].as_array().expect("refs").len(), 301);
    assert!(json.get("result_summary").is_none());
}

/// 上限の不正値は他の入力検証と同じ機械可読エラーで弾く (clap の型エラーにしない)。
///
/// エラーメッセージが名乗る名前は **その面での指定方法**に合わせる — CLI は
/// `--max-results`、session / MCP は `max_results`。利用者が直せない名前を出すと
/// 機械可読エラーの意味がない。
#[test]
fn refs_rejects_invalid_result_limits() {
    for (flag, value, expect) in [
        ("--max-results", "lots", "--max-results"),
        ("--token-budget", "10", "--token-budget"),
        ("--token-budget", "abc", "--token-budget"),
    ] {
        let output = cargo_bin()
            .args(["refs", "--name", "x", "--dir", "src/", flag, value])
            .output()
            .expect("failed to run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
        assert_eq!(
            json["error"]["code"], "INVALID_REQUEST",
            "{flag} {value} -> {stdout}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .expect("message")
                .contains(expect),
            "{flag} {value} -> {stdout}"
        );
    }
}

/// `refs --names` の予算は **呼び出し全体で 1 つ**を round-robin 配分する。
///
/// 名前ごとに上限を課すと全体が名前数に比例して膨らみ、先頭から詰めると高頻度な
/// 1 名が予算を食い尽くして後続が 0 件になる (飢餓)。両方を同じテストで固定する。
#[test]
fn refs_batch_shares_one_budget_round_robin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut src = String::from("pub fn alpha() {}\npub fn beta() {}\n");
    // alpha を圧倒的に多くして、飢餓が起きれば beta が 0 件になる状況を作る。
    for i in 0..200 {
        src.push_str(&format!("pub fn ca{i}() {{ alpha(); }}\n"));
    }
    for i in 0..5 {
        src.push_str(&format!("pub fn cb{i}() {{ beta(); }}\n"));
    }
    std::fs::write(dir.path().join("a.rs"), &src).expect("write");

    let output = cargo_bin()
        .args([
            "refs",
            "--names",
            "alpha,beta",
            "--max-results",
            "20",
            "--token-budget",
            "unlimited",
            "--dir",
        ])
        .arg(dir.path())
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("invalid NDJSON"))
        .collect();
    assert_eq!(records.len(), 2, "名前ごとに 1 レコード: {stdout}");

    let alpha = &records[0];
    let beta = &records[1];
    assert_eq!(alpha["symbol"], "alpha");
    assert_eq!(beta["symbol"], "beta");
    let a_n = alpha["refs"].as_array().expect("refs").len();
    let b_n = beta["refs"].as_array().expect("refs").len();
    // 全体で 20 件を分け合う (名前ごとに 20 ではない)
    assert_eq!(a_n + b_n, 20, "全体で 1 予算: alpha={a_n} beta={b_n}");
    // beta は参照が 6 件しかないので全部出る = 高頻度な alpha に飢餓させられない
    assert_eq!(b_n, 6, "少ない側は飢餓しない: alpha={a_n} beta={b_n}");
    assert!(
        !beta.get("result_summary").is_some_and(|v| !v.is_null()),
        "全件出た側にサマリは付かない: {beta}"
    );
    assert_eq!(alpha["result_summary"]["total"], 201);
}
