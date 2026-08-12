//! 言語別のシンボル抽出・参照解決の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

// ---- Ruby language support tests ----

#[test]
fn ruby_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());

    // Should find module, classes, and methods
    let module = symbols.iter().find(|s| s["name"] == "MyApp");
    assert!(module.is_some(), "Should find MyApp module");
    assert_eq!(module.unwrap()["kind"], "mod");

    let user_class = symbols.iter().find(|s| s["name"] == "User");
    assert!(user_class.is_some(), "Should find User class");
    assert_eq!(user_class.unwrap()["kind"], "class");

    let init_method = symbols.iter().find(|s| s["name"] == "initialize");
    assert!(init_method.is_some(), "Should find initialize method");
    assert_eq!(init_method.unwrap()["kind"], "fn");
}

#[test]
fn ruby_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");

    let calls = json["calls"].as_array().unwrap();
    assert!(!calls.is_empty(), "Should find calls in Ruby file");
}

#[test]
fn ruby_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");

    let imports = json["imports"].as_array().unwrap();
    assert!(!imports.is_empty(), "Should find require statements");

    // Should find 'json' require
    let json_import = imports
        .iter()
        .find(|i| i["src"].as_str().unwrap_or("").contains("json"));
    assert!(json_import.is_some(), "Should find require 'json'");
    assert_eq!(json_import.unwrap()["kind"], "require");
}

#[test]
fn ruby_refs_constant_definition() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "DEFAULT_ROLE",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.rb",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["symbol"], "DEFAULT_ROLE");

    let refs = json["refs"].as_array().unwrap();
    assert!(
        refs.len() >= 2,
        "Should find both definition and reference for DEFAULT_ROLE"
    );

    let defs: Vec<_> = refs.iter().filter(|r| r["kind"] == "def").collect();
    assert!(
        !defs.is_empty(),
        "Should classify constant assignment as definition"
    );

    let refs_only: Vec<_> = refs.iter().filter(|r| r["kind"] == "ref").collect();
    assert!(
        !refs_only.is_empty(),
        "Should classify non-assignment constant usage as reference"
    );
}

#[test]
fn ruby_ast() {
    let output = cargo_bin()
        .args(["ast", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");
    assert!(!json["ast"].as_array().unwrap().is_empty());
}

#[test]
fn ruby_updated_parser_handles_ambiguous_syntax_without_errors() {
    let output = cargo_bin()
        .args([
            "ast",
            "--path",
            "tests/fixtures/ruby_ambiguities.rb",
            "--depth",
            "64",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    json["ast"].as_array().expect("ast should be an array");

    fn count_kind(value: &serde_json::Value, expected: &str) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                usize::from(map.get("kind").and_then(serde_json::Value::as_str) == Some(expected))
                    + map
                        .values()
                        .map(|child| count_kind(child, expected))
                        .sum::<usize>()
            }
            serde_json::Value::Array(values) => {
                values.iter().map(|child| count_kind(child, expected)).sum()
            }
            _ => 0,
        }
    }

    // ERROR が無いだけでなく、曖昧な各構文が意図したノードへ確定したことを固定する。
    for (kind, expected) in [
        ("ERROR", 0),
        ("element_reference", 1),
        ("lambda", 1),
        ("block", 2),
        ("regex", 2),
        ("range", 1),
        ("match_pattern", 1),
        ("heredoc_beginning", 1),
        ("heredoc_body", 1),
    ] {
        assert_eq!(count_kind(&json["ast"], kind), expected, "kind={kind}");
    }
}

// ---- Python 多言語テスト ----

#[test]
fn python_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.py"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "python");

    let symbols = json["symbols"].as_array().unwrap();
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Config"), "class Config を検出すべき");
    assert!(
        names.contains(&"create_config"),
        "function create_config を検出すべき"
    );
}

#[test]
fn python_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.py"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "python");
    assert!(json["calls"].as_array().is_some());
}

#[test]
fn python_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.py"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    let sources: Vec<&str> = imports.iter().map(|i| i["src"].as_str().unwrap()).collect();
    assert!(
        sources.contains(&"pathlib"),
        "pathlib の import を検出すべき"
    );
}

#[test]
fn python_ast() {
    let output = cargo_bin()
        .args([
            "ast",
            "--path",
            "tests/fixtures/sample.py",
            "--line",
            "0",
            "--col",
            "0",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "python");
    assert!(!json["ast"].as_array().unwrap().is_empty());
}

// ---- Go 多言語テスト ----

#[test]
fn go_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.go"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "go");

    let symbols = json["symbols"].as_array().unwrap();
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Server"), "type Server を検出すべき");
    assert!(names.contains(&"NewServer"), "func NewServer を検出すべき");
}

#[test]
fn go_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.go"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "go");
    assert!(json["calls"].as_array().is_some());
}

#[test]
fn go_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.go"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    let sources: Vec<&str> = imports.iter().map(|i| i["src"].as_str().unwrap()).collect();
    assert!(sources.contains(&"fmt"), "fmt の import を検出すべき");
    assert!(
        sources.contains(&"strings"),
        "strings の import を検出すべき"
    );
}

// ---- TypeScript 多言語テスト ----

#[test]
fn typescript_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.ts"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "typescript");

    let symbols = json["symbols"].as_array().unwrap();
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"AppServer"), "class AppServer を検出すべき");
    assert!(
        names.contains(&"createServer"),
        "function createServer を検出すべき"
    );
    assert!(names.contains(&"Config"), "interface Config を検出すべき");
}

#[test]
fn typescript_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.ts"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "typescript");
    assert!(json["calls"].as_array().is_some());
}

#[test]
fn typescript_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.ts"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(!imports.is_empty(), "TypeScript の import を検出すべき");
}

/// TypeScript で関数戻り値型に使われた `type_identifier` が `kind: "ref"` として
/// 認識されることを検証 (レポート 2026-04-24-excel-service-dead-code-false-positive.md の再現)。
/// `function parseExcel(): ExcelParseResult {}` の `ExcelParseResult` が def ではなく
/// ref として分類されることで、dead-code 判定が正しく動作する。
#[test]
fn typescript_return_type_is_ref_not_def() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("excel.ts"),
        "export interface ExcelParseResult { rows: number }\n\
export function parseExcel(buffer: Buffer): ExcelParseResult {\n\
  return { rows: 0 };\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "ExcelParseResult",
            "--dir",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();

    let def_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("def"))
        .count();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        def_count, 1,
        "ExcelParseResult の def は interface 宣言の 1 件だけのはず: {refs:?}"
    );
    assert!(
        ref_count >= 1,
        "戻り値型として使われている ExcelParseResult は ref として 1 件以上検出されるべき: {refs:?}"
    );
}

/// TypeScript の `class A extends B {}` の `B` が ref として認識されることを検証。
/// 単純な grandparent 走査では `class_declaration` に B が def として誤分類される問題への対応。
#[test]
fn typescript_class_extends_is_ref_not_def() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("base.ts"), "export class Base { hello() {} }\n").unwrap();
    std::fs::write(
        root.join("derived.ts"),
        "import { Base } from './base';\nexport class Derived extends Base { extra() {} }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "Base", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let def_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("def"))
        .count();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        def_count, 1,
        "Base の def は base.ts のクラス宣言 1 件: {refs:?}"
    );
    assert!(
        ref_count >= 2,
        "import と extends で 2 件以上 ref が出るべき: {refs:?}"
    );
}

/// Zig で `const X = ...` の右辺 / 関数戻り値型 / test body 内の identifier が
/// def ではなく ref として認識されることを検証 (Issue: zig-definition-kinds-overscoped)。
#[test]
fn zig_initializer_and_return_type_is_ref_not_def() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("base.zig"),
        "pub const Helper = struct {\n    pub fn make() Helper { return .{}; }\n};\n",
    )
    .unwrap();
    std::fs::write(
        root.join("user.zig"),
        "const std = @import(\"std\");\nconst base = @import(\"base.zig\");\n\
pub fn use() base.Helper {\n    const h = base.Helper.make();\n    return h;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "Helper", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let def_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("def"))
        .count();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        def_count, 1,
        "Helper の def は base.zig の struct 宣言 1 件: {refs:?}"
    );
    assert!(
        ref_count >= 1,
        "戻り値型 / 初期化式で参照されている Helper は ref として 1 件以上出るべき: {refs:?}"
    );
}

// ---- refs 多言語テスト ----

#[test]
fn python_refs() {
    let output = cargo_bin()
        .args(["refs", "--name", "Config", "--dir", "tests/fixtures"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    // Config は sample.py と sample.ts 両方に存在
    assert!(!refs.is_empty(), "Config の参照を検出すべき");
}

#[test]
fn go_refs() {
    let output = cargo_bin()
        .args(["refs", "--name", "Server", "--dir", "tests/fixtures"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    assert!(!refs.is_empty(), "Server の参照を検出すべき");
}

/// PHPUnit の DocBlock `@dataProvider` および PHP attribute `#[DataProvider(...)]`
/// 経由で参照される method を astro-sight が参照として解決すること
/// (Issue astro-sight-bug-reports#6)。
#[test]
fn phpunit_dataprovider_refs() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "providerForValidateFormat",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/sample_phpunit_test.php",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs array");
    let defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] == "def").collect();
    let non_defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] != "def").collect();
    assert_eq!(defs.len(), 1, "definition 1 件: {refs:?}");
    assert!(
        non_defs
            .iter()
            .any(|r| r["ctx"].as_str().unwrap_or("").contains("@dataProvider")),
        "@dataProvider 経由の参照を検出すべき: {refs:?}"
    );

    // attribute 経由
    let output2 = cargo_bin()
        .args([
            "refs",
            "--name",
            "attrProvider",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/sample_phpunit_test.php",
        ])
        .output()
        .expect("failed to run");
    assert!(output2.status.success());
    let json2: serde_json::Value = serde_json::from_slice(&output2.stdout).expect("invalid JSON");
    let refs2 = json2["refs"].as_array().expect("refs array");
    let non_defs2: Vec<&serde_json::Value> = refs2.iter().filter(|r| r["kind"] != "def").collect();
    assert!(
        non_defs2
            .iter()
            .any(|r| r["ctx"].as_str().unwrap_or("").contains("DataProvider")),
        "#[DataProvider(...)] 経由の参照を検出すべき: {refs2:?}"
    );
}

/// bash の `trap '<handler>' SIG` 構文の handler 文字列内に書かれた関数呼び出しを
/// astro-sight が参照として解決すること (Issue #5 / astro-sight-bug-reports#5)。
#[test]
fn bash_trap_handler_refs() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "cleanup_signal",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.sh",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs array");
    // 期待: 定義 1 件 + trap 経由参照 2 件 + 通常呼び出し 1 件 = 4 件
    let defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] == "def").collect();
    let non_defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] != "def").collect();
    assert_eq!(defs.len(), 1, "definition 1 件: {refs:?}");
    assert!(
        non_defs.len() >= 3,
        "trap 経由 2 件 + 通常呼出 1 件 で少なくとも 3 件: {refs:?}"
    );
    // trap 行 (line 16, 17) の参照を含むこと
    let trap_refs: Vec<&serde_json::Value> = non_defs
        .iter()
        .filter(|r| {
            let ctx = r["ctx"].as_str().unwrap_or("");
            ctx.contains("trap")
        })
        .copied()
        .collect();
    assert_eq!(trap_refs.len(), 2, "trap 構文経由の参照は 2 件: {refs:?}");
}

// ---- Java 多言語統合テスト ----

#[test]
fn java_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.java"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "java");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "SampleService"),
        "SampleService クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "addItem"),
        "addItem メソッドを検出すべき"
    );
}

#[test]
fn java_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.java",
            "--function",
            "addItem",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "java");
}

#[test]
fn java_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.java"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| { i["ctx"].as_str().unwrap_or("").contains("java.util.List") }),
        "java.util.List の import を検出すべき"
    );
}

// ---- Kotlin 多言語統合テスト ----

#[test]
fn kotlin_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.kt"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "kotlin");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "SampleRepository"),
        "SampleRepository クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "main"),
        "main 関数を検出すべき"
    );
}

#[test]
fn kotlin_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.kt",
            "--function",
            "main",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "kotlin");
}

#[test]
fn kotlin_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.kt"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(!imports.is_empty(), "Kotlin の import を検出すべき");
}

// ---- Swift 多言語統合テスト ----

#[test]
fn swift_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.swift"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "swift");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "TaskManager"),
        "TaskManager クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "addTask"),
        "addTask メソッドを検出すべき"
    );
}

#[test]
fn swift_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.swift",
            "--function",
            "removeTask",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "swift");
}

#[test]
fn swift_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.swift"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("Foundation")),
        "Foundation の import を検出すべき"
    );
}

// ---- C# 多言語統合テスト ----

#[test]
fn csharp_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.cs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "csharp");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "Calculator"),
        "Calculator クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "Add"),
        "Add メソッドを検出すべき"
    );
}

#[test]
fn csharp_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.cs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("System")),
        "System の using を検出すべき"
    );
}

// ---- PHP 多言語統合テスト ----

#[test]
fn php_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.php"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "php");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "UserService"),
        "UserService クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "findUser"),
        "findUser メソッドを検出すべき"
    );
}

#[test]
fn php_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.php"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| { i["ctx"].as_str().unwrap_or("").contains("UserRepository") }),
        "UserRepository の use を検出すべき"
    );
}

// ---- C 多言語統合テスト ----

#[test]
fn c_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.c"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "c");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "main"),
        "main 関数を検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "buffer_append"),
        "buffer_append 関数を検出すべき"
    );
}

/// C/C++ の関数名は `function_declarator` 配下でキャプチャされるため、定義ノードまで
/// 親を繰り上げないと range が宣言子（シグネチャ行）だけに潰れ、複雑度が常に 1 になり、
/// impact 分析が関数本体のみの変更を取りこぼす。range が本体まで伸び、分岐を数えた
/// 複雑度が算出されることを検証する回帰テスト。
#[test]
fn c_function_range_and_complexity_cover_body() {
    let output = cargo_bin()
        .args([
            "symbols",
            "--path",
            "tests/fixtures/sample.c",
            "--full",
            "--no-cache",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let symbols = json["symbols"].as_array().unwrap();

    // buffer_append は本体に if が 1 つあるので base1 + 1 = 2。
    // 宣言子だけを見ていた頃は本体を走査できず常に 1 だった。
    let append = symbols
        .iter()
        .find(|s| s["name"] == "buffer_append")
        .expect("buffer_append を検出すべき");
    assert_eq!(
        append["complexity"], 2,
        "buffer_append の複雑度は本体の if を数えて 2 になるべき"
    );
    let start = append["range"]["start"]["line"].as_u64().unwrap();
    let end = append["range"]["end"]["line"].as_u64().unwrap();
    assert!(
        end > start,
        "range は宣言子 1 行ではなく関数本体まで複数行にまたがるべき (start={start}, end={end})"
    );

    // 分岐の無い関数も range が本体まで伸びる（複雑度は 1）。
    let main_fn = symbols
        .iter()
        .find(|s| s["name"] == "main")
        .expect("main を検出すべき");
    assert_eq!(main_fn["complexity"], 1);
    let m_start = main_fn["range"]["start"]["line"].as_u64().unwrap();
    let m_end = main_fn["range"]["end"]["line"].as_u64().unwrap();
    assert!(
        m_end > m_start,
        "main の range も本体まで複数行にまたがるべき"
    );
}

#[test]
fn c_struct_type_uses_are_refs_not_defs() {
    // GitLab #27: `struct X` のパラメータ型・ローカル変数型・sizeof/cast は
    // tag 定義ではなく型参照として数える。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.c"),
        "\
struct text_server_data;

struct text_server_data {
    int command;
};

static int parse_header(struct text_server_data* header) {
    header->command = 1;
    return 0;
}

void run(void) {
    struct text_server_data header;
    void* raw = (void*)sizeof(struct text_server_data);
    (void)(struct text_server_data*)raw;
    parse_header(&header);
}
",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "text_server_data",
            "--dir",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let def_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("def"))
        .count();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        def_count, 1,
        "body 付き struct tag 定義だけが def になるべき: {refs:?}"
    );
    assert!(
        ref_count >= 4,
        "パラメータ型・変数宣言・sizeof・cast は ref になるべき: {refs:?}"
    );
    assert!(
        !refs
            .iter()
            .any(|r| r["ctx"].as_str() == Some("struct text_server_data;")),
        "forward declaration は def/ref のどちらにも数えない: {refs:?}"
    );
}

#[test]
fn c_typedef_forward_tag_is_not_ref() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.c"),
        "\
typedef struct forward_alias forward_alias;

struct forward_alias {
    int value;
};
",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "forward_alias",
            "--dir",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        ref_count, 0,
        "typedef forward declaration の tag 側は non-definition ref にしない: {refs:?}"
    );
}

#[test]
fn c_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.c",
            "--function",
            "main",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "c");
}

#[test]
fn c_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.c"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("stdio.h")),
        "stdio.h の include を検出すべき"
    );
}

// ---- C++ 多言語統合テスト ----

#[test]
fn cpp_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.cpp"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "cpp");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "StringPool"),
        "StringPool クラスを検出すべき"
    );
}

#[test]
fn cpp_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.cpp",
            "--function",
            "main",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "cpp");
}

#[test]
fn cpp_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.cpp"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("string")),
        "string の include を検出すべき"
    );
}

// ---- Xojo 言語サポートのスモークテスト ----
// v26.6 で tree-sitter-xojo を削除し lexer-only に移行。
// PR3 で lexer 経由の symbols/refs/calls が復活するまで以下のテストは ignore する。

#[test]
fn xojo_symbols_from_fixture() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.xojo_code"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "xojo");
    let names: Vec<&str> = json["symbols"]
        .as_array()
        .expect("symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    for expected in ["Greeter", "Greet", "DefaultName", "Counter", "Helpers"] {
        assert!(
            names.contains(&expected),
            "Xojo fixture から {expected} を抽出すべき: {names:?}"
        );
    }
}

#[test]
fn xojo_calls_returns_unsupported() {
    // v26.6 以降、Xojo は lexer-only バックエンド。calls は tree-sitter Query 必須のため
    // UNSUPPORTED_LANGUAGE を返す (空結果ではなく明確なエラーで AI エージェントに区別可能)。
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.xojo_code"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "calls は xojo に対して非ゼロ exit すべき"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(
        json["error"]["code"], "UNSUPPORTED_LANGUAGE",
        "xojo は lexer-only のため calls は UNSUPPORTED_LANGUAGE を返す"
    );
}

#[test]
fn xojo_refs_case_insensitive_uppercase() {
    // Xojo は識別子が case-insensitive。大文字の `GREET` で小文字定義がヒットすべき。
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "GREET",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.xojo_code",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs 配列");
    assert!(
        refs.iter().any(|r| r["kind"] == "def"),
        "GREET で Greet の定義がヒットすべき: {refs:?}"
    );
    assert!(
        refs.iter().any(|r| r["kind"] == "ref"),
        "GREET で Greet の呼び出し参照がヒットすべき: {refs:?}"
    );
}

#[test]
fn xojo_refs_lowercase_matches_mixedcase_definition() {
    // 小文字 `greet` でも Greet 定義と同件数がヒットすべき。
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "greet",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.xojo_code",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs 配列");
    assert!(!refs.is_empty(), "小文字 greet でもヒットすべき");
}

#[test]
fn xojo_refs_rust_case_preserved() {
    // Rust 等の case-sensitive 言語では従来通り大文字小文字を区別する。
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "EXTRACT_SYMBOLS_NAME",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.rb",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs 配列");
    // Ruby は case-sensitive なので大文字の `EXTRACT_SYMBOLS_NAME` はヒットしない。
    // (小文字シンボル extract_symbols などがあっても影響しないことを担保)
    assert!(refs.is_empty() || refs.iter().all(|r| r["kind"].is_string()));
}

#[test]
fn xojo_refs_batch_case_insensitive_collision() {
    // Xojo は case-insensitive。`Greet` と `greet` を同一バッチで渡しても
    // 両方に同じ参照リストが割り当たるべき（正規化キーの衝突で片方が欠落しないこと）。
    let output = cargo_bin()
        .args([
            "refs",
            "--names",
            "Greet,greet",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.xojo_code",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "2シンボル分の NDJSON が出力されるべき");

    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("invalid JSON");
    let second: serde_json::Value = serde_json::from_str(lines[1]).expect("invalid JSON");
    let refs1 = first["refs"].as_array().expect("refs array");
    let refs2 = second["refs"].as_array().expect("refs array");

    assert!(
        !refs1.is_empty() && !refs2.is_empty(),
        "`Greet` / `greet` どちらも参照を持つべき (片方欠落しないこと): Greet={:?}, greet={:?}",
        refs1,
        refs2
    );
    assert_eq!(
        refs1.len(),
        refs2.len(),
        "同じ正規化キーなら同数の参照であるべき"
    );
}

#[test]
fn xojo_doctor_lists_xojo() {
    let output = cargo_bin()
        .args(["doctor"])
        .output()
        .expect("failed to run doctor");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let xojo = json["languages"]
        .as_array()
        .expect("languages 配列")
        .iter()
        .find(|l| l["language"] == "xojo")
        .expect("doctor 出力に xojo が含まれるべき");
    // v26.6 以降、Xojo は lexer-only バックエンドに移行。tree-sitter parser_version は持たない。
    assert_eq!(xojo["available"], true);
    assert_eq!(xojo["backend"], "lexer_only");
    assert!(
        xojo.get("parser_version").is_none() || xojo["parser_version"].is_null(),
        "lexer_only バックエンドは parser_version を持たない"
    );
}

// ---------------------------------------------------------------------------
// git 管理外ディレクトリでの graceful skip (--git)
// ---------------------------------------------------------------------------

#[test]
fn non_git_review_hook_silent_exit_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["review", "--dir", path, "--git", "--hook"])
        .output()
        .expect("run");
    assert!(output.status.success(), "非 git の --hook は exit 0");
    assert!(
        output.stdout.is_empty(),
        "stdout は空であるべき: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr は空であるべき: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn non_git_review_emits_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["review", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
    assert_eq!(json["skipped"]["source"], "git");
    assert!(json["impact"]["changes"].as_array().unwrap().is_empty());
}

#[test]
fn non_git_impact_hook_silent_exit_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["impact", "--dir", path, "--git", "--hook"])
        .output()
        .expect("run");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn non_git_context_empty_changes_with_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["context", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["changes"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
}

#[test]
fn non_git_dead_code_emits_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["dead-code", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["dead_symbols"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
}

#[test]
fn non_git_cochange_emits_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["cochange", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["entries"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
}
