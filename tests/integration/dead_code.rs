//! dead-code 検出の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn dead_code_dynamic_import_default_is_live_without_synthetic_refs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("lib")).unwrap();
    std::fs::write(
        root.join("lib/panel.tsx"),
        "export default function Panel() { return <div/>; }\nexport function unusedNamed() {}",
    )
    .unwrap();
    std::fs::write(
        root.join("app.tsx"),
        "export async function loadPanel() {\n\
         const module = await import('./lib/panel.js');\n\
         return module.default;\n\
         }\nvoid loadPanel();",
    )
    .unwrap();
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--glob",
            "lib/**",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let dead = json["dead_symbols"].as_array().unwrap();
    assert_eq!(dead.len(), 1, "{json}");
    assert_eq!(dead[0]["name"], "unusedNamed");

    for option in ["--name", "--names"] {
        let output = cargo_bin()
            .args(["refs", option, "Panel", "--dir", root.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let refs = json["refs"].as_array().unwrap();
        assert_eq!(refs.len(), 1, "{json}");
        assert_eq!(refs[0]["kind"], "def");
    }
}

#[test]
fn review_hook_keeps_a_changed_dynamic_default_export_out_of_dead() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "panel.tsx",
        "export default function Panel() { return <div/>; }\n",
    );
    repo.write(
        "app.ts",
        "export function load() { return import('./panel.js'); }\nvoid load();\n",
    );
    repo.commit_all("baseline");
    repo.write(
        "panel.tsx",
        "export default function Panel() { return <section/>; }\n",
    );

    let dead = repo.run_json("dead-code", &["--git"]);
    assert!(
        dead["dead_symbols"].as_array().unwrap().is_empty(),
        "{dead}"
    );
    let review = repo.run_json("review", &["--git"]);
    assert!(
        review["dead_symbols"].as_array().unwrap().is_empty(),
        "{review}"
    );
    let hook = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    assert!(hook.stdout.is_empty());
}

#[test]
fn dead_code_does_not_exclude_angular_provider_callback_from_sibling_token() {
    // codex review: 同じファイルに RECAPTCHA_LOADER_OPTIONS import/provider があっても、
    // OTHER_TOKEN の provider object にある onBeforeLoad は除外しない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("src/app/app.component.ts"),
        "\
import { Component } from '@angular/core';
import { RECAPTCHA_LOADER_OPTIONS } from 'ng-recaptcha-2';
@Component({
  template: '',
  providers: [{
    provide: RECAPTCHA_LOADER_OPTIONS,
    useValue: {}
  }, {
    provide: OTHER_TOKEN,
    useValue: {
      onBeforeLoad(url: URL) { return url; },
    }
  }]
})
export class AppComponent {}
",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        dead_names.iter().any(|n| n.contains("onBeforeLoad")),
        "RECAPTCHA_LOADER_OPTIONS 以外の provider callback は dead として残るべき: {dead_names:?}"
    );
}

// ---- dead-code サブコマンドテスト ----

#[test]
fn dead_code_on_fixtures() {
    let output = cargo_bin()
        .args(["dead-code", "--dir", "tests/fixtures"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(json["dir"].as_str().is_some());
    assert!(json["scanned_files"].as_u64().is_some());
    assert!(json["dead_symbols"].as_array().is_some());
}

/// `--framework laravel` 明示指定時は package.json の next 依存があっても nextjs
/// auto-detect は発動しない (明示指定が常に優先)。
#[test]
fn dead_code_explicit_framework_overrides_auto_detect() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "demo", "dependencies": { "next": "^15.0.0" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/page.tsx"),
        "export default function OverriddenPage() { return null; }\n",
    )
    .unwrap();

    // --framework laravel を明示指定 → next auto-detect は発動せず page.tsx は dead 判定のまま
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "OverriddenPage"),
        "--framework laravel 明示指定時は nextjs auto-detect が発動しない (明示指定が常に優先): {names:?}"
    );
}

#[test]
fn dead_code_refs_scope_not_limited_by_glob() {
    // F3 回帰テスト: `--glob` で symbols 対象ファイルを絞っても、
    // refs 探索は `--dir` 全体で行われ、`--glob` 範囲外からの参照でも
    // dead 判定を回避できること。
    //
    // 従来は `detect_dead_symbols_from_files` が refs 探索にも `--glob` を
    // 適用していたため、`--glob 'lib/**/*.rs'` で走らせると `app/` からの
    // 参照が見えず、lib/ 配下の共通関数が誤って dead 扱いになっていた。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    // lib 配下: 共通関数の定義
    std::fs::write(
        root.join("lib/util.rs"),
        "pub fn shared_helper() -> i32 { 42 }\n",
    )
    .unwrap();
    // app 配下: lib の関数を呼ぶ (refs スコープを広げれば見える)
    std::fs::write(
        root.join("app/main.rs"),
        "fn main() { let _ = shared_helper(); }\n",
    )
    .unwrap();

    // --glob で lib/ 配下のみを dead 対象にするが、refs は root 全体で探索されるべき
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--glob",
            "lib/**/*.rs",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("shared_helper")),
        "shared_helper は app/ から参照されており、--glob が refs スコープを狭めるべきでない (F3 regression): {names:?}"
    );
}

#[test]
fn dead_code_diff_hidden_candidate_files_are_in_ref_scope() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".hidden/scripts")).unwrap();
    std::fs::write(
        root.join(".hidden/scripts/tool.sh"),
        "guard() { return 0; }\nmain() { guard || status=$?; }\nmain\n",
    )
    .unwrap();
    let diff = root.join("change.diff");
    std::fs::write(
        &diff,
        "diff --git a/.hidden/scripts/tool.sh b/.hidden/scripts/tool.sh\n\
new file mode 100755\n\
--- /dev/null\n\
+++ b/.hidden/scripts/tool.sh\n\
@@ -0,0 +1,3 @@\n\
+guard() { return 0; }\n\
+main() { guard || status=$?; }\n\
+main\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|name| name == "guard" || name == "main"),
        "hidden 配下の diff 候補ファイル内参照も集計されるべき: {names:?}"
    );
}

#[test]
fn dead_code_test_only_symbols_separated_from_dead() {
    // F5: production からは参照されず test/ からのみ参照されるシンボルは
    // dead_symbols ではなく test_only_symbols バケットに分類されること。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();

    // src/lib.rs: production helper (test からだけ呼ばれる) と really_dead (誰からも呼ばれない)
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn used_in_test_only() -> i32 { 1 }\npub fn really_dead() -> i32 { 2 }\n",
    )
    .unwrap();
    // tests/it.rs: used_in_test_only を参照する (production 側 src/ からは未参照)
    std::fs::write(
        root.join("tests/it.rs"),
        "use foo::used_in_test_only;\n#[test]\nfn t() { assert_eq!(used_in_test_only(), 1); }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    let test_only: Vec<String> = json
        .get("test_only_symbols")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    assert!(
        dead.iter().any(|n| n.contains("really_dead")),
        "really_dead は production / test 双方から参照されないので dead_symbols に出るべき: dead={dead:?}"
    );
    assert!(
        !dead.iter().any(|n| n.contains("used_in_test_only")),
        "used_in_test_only は test/ から参照されるので dead_symbols から外れるべき: dead={dead:?}"
    );
    assert!(
        test_only.iter().any(|n| n.contains("used_in_test_only")),
        "used_in_test_only は test_only_symbols バケットに含まれるべき: test_only={test_only:?}"
    );
}

#[test]
fn dead_code_unknown_framework_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("sample.php"), "<?php\n").unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "djangular",
        ])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "未知の framework 名はエラー (exit != 0)"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Unknown framework preset") || stdout.contains("INVALID_REQUEST"),
        "エラーメッセージに framework 未対応が示される: {stdout}"
    );
}

#[test]
fn dead_code_same_method_name_in_multiple_classes_skipped() {
    // 同名メソッドが複数クラスに存在する場合、bare name では区別できないため
    // 保守的に dead 判定から除外されることを確認する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        "class Alpha:\n    def run(self):\n        pass\n\nclass Beta:\n    def run(self):\n        pass\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".run")),
        "同名メソッドが複数クラスにある場合はスキップされるべき: {names:?}"
    );
}

#[test]
fn dead_code_rust_same_named_methods_keep_only_the_used_owner_live() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod used; pub mod unused; pub mod call;\n",
    );
    repo.write(
        "src/used.rs",
        "pub struct ReadCursor; impl ReadCursor { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/unused.rs",
        "pub struct WriteCursor; impl WriteCursor { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/call.rs",
        "use crate::used::ReadCursor; pub fn call() { let mut value = ReadCursor; value.put_u16(); }\n",
    );

    let result = repo.run_json("dead-code", &[]);
    let names: Vec<&str> = result["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        names.contains(&"WriteCursor.put_u16"),
        "unused owner must be reported: {result}"
    );
    assert!(
        !names.contains(&"ReadCursor.put_u16"),
        "called owner must stay live: {result}"
    );

    repo.write(
        "src/call.rs",
        "use crate::used::ReadCursor; pub fn call() { let mut value = ReadCursor; value.r#put_u16(); }\n",
    );
    let raw_name = repo.run_json("dead-code", &[]);
    assert!(
        !raw_name["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "ReadCursor.put_u16"),
        "{raw_name}"
    );
    assert!(
        raw_name["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "WriteCursor.put_u16"),
        "{raw_name}"
    );

    repo.create_dir_all(".cache/worktree");
    repo.write(".cache/worktree/.git", "gitdir: elsewhere\n");
    repo.write(
        ".cache/worktree/Cargo.toml",
        "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let with_separate_checkout = repo.run_json("dead-code", &[]);
    assert!(
        with_separate_checkout["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "WriteCursor.put_u16"),
        "{with_separate_checkout}"
    );
}

#[test]
fn dead_code_rust_free_function_calls_do_not_keep_same_named_method_live() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod cursor; pub mod encoding; pub mod consumer;\n",
    );
    repo.write(
        "src/cursor.rs",
        "pub struct WriteCursor<'a>(&'a mut [u8]); impl<'a> WriteCursor<'a> { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/encoding.rs",
        "macro_rules! unrelated { () => { 1 } } pub fn put_u16() {} pub fn encode() { let _ = unrelated!(); put_u16(); }\n",
    );
    repo.write(
        "src/consumer.rs",
        "use crate::encoding::put_u16; pub fn consume() { put_u16(); }\n",
    );

    let result = repo.run_json("dead-code", &[]);
    let names: Vec<&str> = result["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        names.contains(&"WriteCursor.put_u16"),
        "free function calls cannot keep the method live: {result}"
    );
    assert!(
        !names.contains(&"put_u16"),
        "called free function must stay live: {result}"
    );

    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[[bin]]\nname = \"sample-bin\"\npath = \"src/main.rs\"\n",
    );
    repo.write("src/main.rs", "fn main() {}\n");
    let with_binary = repo.run_json("dead-code", &[]);
    assert!(
        with_binary["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "WriteCursor.put_u16"),
        "{with_binary}"
    );
}

#[test]
fn dead_code_rust_unknown_receiver_keeps_duplicate_methods_unclassified() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod first; pub mod second; pub mod call;\n",
    );
    repo.write(
        "src/first.rs",
        "pub struct First; impl First { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/second.rs",
        "pub struct Second; impl Second { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/call.rs",
        "use crate::first::First; pub fn apply(value: &mut First) { value.put_u16(); }\n",
    );

    let result = repo.run_json("dead-code", &[]);
    let names: Vec<&str> = result["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        !names.iter().any(|name| name.ends_with(".put_u16")),
        "unknown receiver must keep both candidates unclassified: {result}"
    );
}

#[test]
fn dead_code_rust_generated_calls_without_literal_method_name_are_uncertain() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod a; pub mod b; pub mod generated;\n");
    repo.write(
        "src/a.rs",
        "pub struct A; impl A { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/b.rs",
        "pub struct B; impl B { pub fn put_u16(&mut self) {} }\n",
    );
    let baseline = repo.run_json("dead-code", &[]);
    assert!(
        baseline["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "A.put_u16"),
        "{baseline}"
    );

    repo.write("src/generated.rs", "// only a comment: [<put_ u16>]\n");
    let comment_only = repo.run_json("dead-code", &[]);
    assert!(
        comment_only["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "A.put_u16"),
        "{comment_only}"
    );

    for generated in [
        "include!(concat!(env!(\"OUT_DIR\"), \"/calls.rs\"));\n",
        "std::include ! (concat!(env!(\"OUT_DIR\"), \"/calls.rs\"));\n",
        "use std::include as load; load!(concat!(env!(\"OUT_DIR\"), \"/calls.rs\"));\n",
        "macro_rules! make { ($t:ident) => { paste::paste! { fn call() { let _ = [<put_ $t>]; } } } }\n",
    ] {
        repo.write("src/generated.rs", generated);
        let result = repo.run_json("dead-code", &[]);
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".put_u16"))),
            "{result}"
        );
    }
}

#[test]
fn dead_code_rust_attribute_string_method_names_are_uncertain() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod a; pub mod b; pub mod config;\n");
    repo.write(
        "src/a.rs",
        "pub struct A; impl A { pub fn is_default(&self) -> bool { true } }\n",
    );
    repo.write(
        "src/b.rs",
        "pub struct B; impl B { pub fn is_default(&self) -> bool { true } }\n",
    );
    repo.write(
        "src/config.rs",
        "#[serde(skip_serializing_if = \"A::is_default\")] pub struct Config;\n",
    );
    let result = repo.run_json("dead-code", &[]);
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".is_default"))),
        "{result}"
    );
}

#[test]
fn dead_code_rust_unique_unknown_receiver_preserves_test_only_result() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.create_dir_all("tests");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod store;\n");
    repo.write(
        "src/store.rs",
        "pub struct Store; impl Store { pub fn ping(&self) {} }\n",
    );
    repo.write(
        "tests/call.rs",
        "use sample::store::Store; fn check() { let value = Store; let _ = 1; value.ping(); }\n",
    );
    let result = repo.run_json("dead-code", &[]);
    assert!(
        result["test_only_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "Store.ping"),
        "{result}"
    );
}

#[test]
fn dead_code_rust_workspace_proc_macro_keeps_generated_calls_uncertain() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.create_dir_all("macros");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod a; pub mod b;\n");
    repo.write(
        "src/a.rs",
        "pub struct A; impl A { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/b.rs",
        "pub struct B; impl B { pub fn put_u16(&mut self) {} }\n",
    );
    let baseline = repo.run_json("dead-code", &[]);
    assert!(
        baseline["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "A.put_u16"),
        "{baseline}"
    );

    for key in ["proc-macro", "proc_macro"] {
        repo.write("macros/Cargo.toml", format!("[package]\nname = \"sample-macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\n{key} = true\n"));
        let result = repo.run_json("dead-code", &[]);
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".put_u16"))),
            "{result}"
        );
    }
    repo.write(
        "macros/Cargo.toml",
        "[package]\nname = \"sample-macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("Cargo.toml", "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshared = { path = \"../shared\" }\n");
    let path_dependency = repo.run_json("dead-code", &[]);
    assert!(
        !path_dependency["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".put_u16"))),
        "{path_dependency}"
    );
}

#[test]
fn dead_code_rust_exported_impl_attribute_is_not_classified() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod reader; pub mod writer;\n");
    repo.write(
        "src/reader.rs",
        "pub struct Reader; impl Reader { pub fn read(&self) {} }\n",
    );
    repo.write(
        "src/writer.rs",
        "pub struct Writer; impl Writer { pub fn read(&self) {} }\n",
    );
    let baseline = repo.run_json("dead-code", &[]);
    assert!(
        baseline["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "Reader.read"),
        "{baseline}"
    );

    for attribute in [
        "#[pymethods]",
        "#[cfg_attr(feature = \"python\", pymethods)]",
    ] {
        repo.write(
            "src/reader.rs",
            format!("pub struct Reader; {attribute} impl Reader {{ pub fn read(&self) {{}} }}\n"),
        );
        let result = repo.run_json("dead-code", &[]);
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".read"))),
            "{result}"
        );
    }
}

#[test]
fn dead_code_rust_same_name_const_does_not_prove_receiver_owner() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod used; pub mod writer; pub mod call;\n",
    );
    repo.write(
        "src/writer.rs",
        "pub struct WriteCursor; impl WriteCursor { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write("src/used.rs", "use crate::writer::WriteCursor; pub struct ReadCursor { x: u8 } impl ReadCursor { pub fn put_u16(&mut self) {} } #[allow(non_upper_case_globals)] pub const ReadCursor: WriteCursor = WriteCursor;\n");
    repo.write("src/call.rs", "use crate::used::ReadCursor; pub fn call() { let mut value = ReadCursor; value.put_u16(); }\n");
    let result = repo.run_json("dead-code", &[]);
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".put_u16"))),
        "{result}"
    );
}

#[test]
fn dead_code_rust_nested_package_keeps_sibling_macro_source_uncertain() {
    let repo = TestRepo::new();
    repo.create_dir_all("app/src");
    repo.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("app/src/lib.rs", "pub mod a; pub mod b;\n");
    repo.write(
        "app/src/a.rs",
        "pub struct A; impl A { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "app/src/b.rs",
        "pub struct B; impl B { pub fn put_u16(&mut self) {} }\n",
    );
    let app = repo.root().join("app");
    let run = || {
        let output = cargo_bin()
            .args(["dead-code", "--dir", app.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let baseline = run();
    assert!(
        baseline["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "A.put_u16"),
        "{baseline}"
    );

    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"macros\"]\n",
    );
    let result = run();
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".put_u16"))),
        "{result}"
    );
}

#[test]
fn dead_code_rust_nested_import_does_not_bind_outer_receiver() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod aliases; pub mod call; pub mod used; pub mod writer;\n",
    );
    repo.write(
        "src/used.rs",
        "pub struct ReadCursor; impl ReadCursor { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/writer.rs",
        "pub struct WriteCursor; impl WriteCursor { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/aliases.rs",
        "pub use crate::writer::WriteCursor as ReadCursor;\n",
    );
    repo.write("src/call.rs", "use crate::aliases::*; pub fn call() { let mut value = ReadCursor; value.put_u16(); } #[cfg(test)] mod tests { use super::*; use crate::used::ReadCursor; #[test] fn check() {} }\n");
    let result = repo.run_json("dead-code", &[]);
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".put_u16"))),
        "{result}"
    );
}

#[test]
fn dead_code_rust_binary_module_does_not_use_library_owner() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod used; pub mod writer;\n");
    repo.write("src/main.rs", "mod writer; mod used { pub use crate::writer::WriteCursor as ReadCursor; } mod call; fn main() { call::call(); }\n");
    repo.write(
        "src/used.rs",
        "pub struct ReadCursor; impl ReadCursor { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write(
        "src/writer.rs",
        "pub struct WriteCursor; impl WriteCursor { pub fn put_u16(&mut self) {} }\n",
    );
    repo.write("src/call.rs", "use crate::used::ReadCursor; pub fn call() { let mut value = ReadCursor; value.put_u16(); }\n");
    let result = repo.run_json("dead-code", &[]);
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".put_u16"))),
        "{result}"
    );
}

#[test]
fn dead_code_rust_explicit_target_roots_keep_other_crate_calls_uncertain() {
    for target in ["example", "test", "bench", "build"] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        let declaration = if target == "build" {
            "build = \"src/alt.rs\"".to_owned()
        } else {
            format!("\n[[{target}]]\nname = \"alt\"\npath = \"src/alt.rs\"")
        };
        repo.write("Cargo.toml", format!("[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{declaration}\n"));
        repo.write(
            "src/lib.rs",
            "pub mod used; pub mod writer; pub mod call;\n",
        );
        repo.write("src/alt.rs", "mod writer; mod used { pub use crate::writer::WriteCursor as ReadCursor; } mod call; fn main() { call::call(); }\n");
        repo.write(
            "src/used.rs",
            "pub struct ReadCursor; impl ReadCursor { pub fn put_u16(&mut self) {} }\n",
        );
        repo.write(
            "src/writer.rs",
            "pub struct WriteCursor; impl WriteCursor { pub fn put_u16(&mut self) {} }\n",
        );
        repo.write("src/call.rs", "use crate::used::ReadCursor; pub fn call() { let mut value = ReadCursor; value.put_u16(); }\n");
        let result = repo.run_json("dead-code", &[]);
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".put_u16"))),
            "{target}: {result}"
        );
    }
}

#[test]
fn dead_code_rust_path_attribute_keeps_unscanned_calls_uncertain() {
    for attribute in [
        "#[path = \"calls.inc\"]",
        "# [path = \"calls.inc\"]",
        "#[path /*note*/ = \"calls.inc\"]",
        "#[cfg_attr(all(), path = \"calls.inc\")]",
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write(
            "src/lib.rs",
            format!("pub mod a; pub mod b; {attribute} mod calls;\n"),
        );
        repo.write(
            "src/a.rs",
            "pub struct A; impl A { pub fn put_u16(&mut self) {} }\n",
        );
        repo.write(
            "src/b.rs",
            "pub struct B; impl B { pub fn put_u16(&mut self) {} }\n",
        );
        repo.write(
            "src/calls.inc",
            "pub fn call() { let mut value = crate::b::B; value.put_u16(); }\n",
        );
        let result = repo.run_json("dead-code", &[]);
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".put_u16"))),
            "{attribute}: {result}"
        );
    }
}

#[test]
fn dead_code_rust_cfg_switched_owner_keeps_live_method_uncertain() {
    for conditional_struct in [true, false] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write("Cargo.toml", "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nfast = []\n");
        repo.write(
            "src/lib.rs",
            "pub mod used; pub mod writer; pub mod call;\n",
        );
        repo.write(
            "src/writer.rs",
            "pub struct WriteCursor; impl WriteCursor { pub fn put_u16(&self) {} }\n",
        );
        if conditional_struct {
            repo.write("src/used.rs", "#[cfg(feature = \"fast\")] pub struct ReadCursor; #[cfg(feature = \"fast\")] impl ReadCursor { pub fn put_u16(&self) {} } #[cfg(not(feature = \"fast\"))] pub use crate::writer::WriteCursor as ReadCursor;\n");
        } else {
            repo.write("src/used.rs", "use crate::writer::WriteCursor; pub struct ReadCursor; #[cfg(feature = \"fast\")] impl ReadCursor { pub fn put_u16(&self) {} } #[cfg(not(feature = \"fast\"))] impl std::ops::Deref for ReadCursor { type Target = WriteCursor; fn deref(&self) -> &WriteCursor { &WriteCursor } }\n");
        }
        repo.write("src/call.rs", "use crate::used::ReadCursor; pub fn call() { let value = ReadCursor; value.put_u16(); }\n");
        let result = repo.run_json("dead-code", &[]);
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".put_u16"))),
            "conditional_struct={conditional_struct}: {result}"
        );
    }
}

#[test]
fn dead_code_rust_inner_cfg_keeps_live_deref_method_uncertain() {
    for inner_in_impl in [true, false] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write("Cargo.toml", "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nfast = []\n");
        repo.write(
            "src/lib.rs",
            "pub mod used; pub mod writer; pub mod call;\n",
        );
        repo.write(
            "src/writer.rs",
            "pub struct WriteCursor; impl WriteCursor { pub fn put_u16(&self) {} }\n",
        );
        let method = if inner_in_impl {
            "impl ReadCursor { #![cfg(feature = \"fast\")] pub fn put_u16(&self) {} }"
        } else {
            "impl ReadCursor { pub fn put_u16(&self) { #![cfg(feature = \"fast\")] } }"
        };
        repo.write("src/used.rs", format!("use crate::writer::WriteCursor; pub struct ReadCursor; {method} #[cfg(not(feature = \"fast\"))] impl std::ops::Deref for ReadCursor {{ type Target = WriteCursor; fn deref(&self) -> &WriteCursor {{ &WriteCursor }} }}\n"));
        repo.write("src/call.rs", "use crate::used::ReadCursor; pub fn call() { let value = ReadCursor; value.put_u16(); }\n");
        let result = repo.run_json("dead-code", &[]);
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".put_u16"))),
            "inner_in_impl={inner_in_impl}: {result}"
        );
    }
}

#[test]
fn dead_code_rust_ignored_compiled_source_keeps_calls_uncertain() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(".gitignore", "src/generated.rs\n");
    repo.write("src/lib.rs", "pub mod a; pub mod b; pub mod generated;\n");
    repo.write(
        "src/a.rs",
        "pub struct A; impl A { pub fn put_u16(&self) {} }\n",
    );
    repo.write(
        "src/b.rs",
        "pub struct B; impl B { pub fn put_u16(&self) {} }\n",
    );
    repo.write(
        "src/generated.rs",
        "pub fn call() { crate::b::B.put_u16(); }\n",
    );
    let result = repo.run_json("dead-code", &[]);
    assert_eq!(result["scanned_files"], 3, "{result}");
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".put_u16"))),
        "{result}"
    );
}

#[test]
fn dead_code_rust_raw_method_call_stays_live() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod a; pub mod call;\n");
    repo.write(
        "src/a.rs",
        "pub struct A; impl A { pub fn r#type(&self) -> u8 { 0 } }\n",
    );
    repo.write(
        "src/call.rs",
        "pub fn call(a: &crate::a::A) -> u8 { a.r#type() }\n",
    );
    let result = repo.run_json("dead-code", &[]);
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "A.r#type"),
        "{result}"
    );
}

#[test]
fn dead_code_rust_ignored_nested_checkout_under_src_keeps_calls_uncertain() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src/gen");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(".gitignore", "src/gen/\n");
    repo.write("src/lib.rs", "pub mod a; pub mod b; pub mod gen;\n");
    repo.write(
        "src/a.rs",
        "pub struct A; impl A { pub fn put_u16(&self) {} }\n",
    );
    repo.write(
        "src/b.rs",
        "pub struct B; impl B { pub fn put_u16(&self) {} }\n",
    );
    repo.write("src/gen/.git", "gitdir: elsewhere\n");
    repo.write(
        "src/gen/mod.rs",
        "pub fn call() { crate::b::B.put_u16(); }\n",
    );
    let result = repo.run_json("dead-code", &[]);
    assert_eq!(result["scanned_files"], 3, "{result}");
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(".put_u16"))),
        "{result}"
    );
}

#[test]
fn dead_code_rust_explicit_root_outside_standard_sources_keeps_calls_uncertain() {
    for library in [true, false] {
        let repo = TestRepo::new();
        repo.init_git();
        let prefix = if library { "" } else { "tools/" };
        let root_file = if library { "lib.rs" } else { "tools/main.rs" };
        let target = if library {
            "[lib]\npath = \"lib.rs\"\n"
        } else {
            "[[bin]]\nname = \"sample-bin\"\npath = \"tools/main.rs\"\n"
        };
        repo.create_dir_all(format!("{prefix}gen"));
        repo.write(
            "Cargo.toml",
            format!(
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{target}"
            ),
        );
        repo.write(".gitignore", format!("{prefix}gen/\n"));
        let main = if library {
            "pub mod a; pub mod b; pub mod gen;\n"
        } else {
            "mod a; mod b; mod gen; fn main() {}\n"
        };
        repo.write(root_file, main);
        repo.write(
            format!("{prefix}a.rs"),
            "pub struct A; impl A { pub fn put_u16(&self) {} }\n",
        );
        repo.write(
            format!("{prefix}b.rs"),
            "pub struct B; impl B { pub fn put_u16(&self) {} }\n",
        );
        repo.write(format!("{prefix}gen/.git"), "gitdir: elsewhere\n");
        repo.write(
            format!("{prefix}gen/mod.rs"),
            "pub fn call() { crate::b::B.put_u16(); }\n",
        );
        let result = repo.run_json("dead-code", &[]);
        assert_eq!(result["scanned_files"], 3, "{result}");
        assert!(
            !result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(".put_u16"))),
            "library={library}: {result}"
        );
    }
}

#[test]
fn dead_code_layout_onclick_references_handler() {
    // layout XML の `android:onClick="handler"` から Kotlin/Java のメソッドが
    // 呼ばれるため、そのハンドラは dead 扱いすべきでない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("AndroidManifest.xml"),
        r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android"><application><activity android:name=".MainActivity"/></application></manifest>"#,
    )
    .unwrap();
    std::fs::write(
        root.join("activity_main.xml"),
        r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android">
  <Button android:onClick="onSubmit" android:id="@+id/btn"/>
</LinearLayout>
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("MainActivity.kt"),
        r#"package com.example

class MainActivity : AppCompatActivity() {
    fun onSubmit(view: View) {}
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".onSubmit")),
        "layout XML の android:onClick で参照されたハンドラは dead 扱いすべきでない: {names:?}"
    );
}

/// 2026-05-27 zod-inferred-types-pre-existing-dead 対応: `--dead-scope touched-symbols` の
/// 回帰テスト。changed file 内に元から存在する dead は除外し、今回の hunk に被るシンボル
/// だけが返ることを検証。`review --hook` のデフォルト挙動でもある。
#[test]
fn dead_scope_touched_symbols_excludes_pre_existing_dead() {
    use std::path::Path;
    use std::process::Command;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let src_dir = root.join("src");
    std::fs::create_dir(&src_dir).expect("create src");

    // lib.rs: 公開 module 宣言だけ。dead 検出の対象は src/foo.rs のシンボル。
    std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"demo\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[lib]\npath=\"src/lib.rs\"\n").unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub mod foo;\n").unwrap();
    // 初期コミット: ExistingDead と Used が両方未参照 (本来両方 dead 候補)。
    std::fs::write(
        src_dir.join("foo.rs"),
        "pub fn existing_dead() {}\n\npub fn used() {}\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let s = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(s.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "x@y"]);
    git(&["config", "user.name", "x"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // 新規 hunk: src/foo.rs の末尾に `new_dead` を追加。`existing_dead` の宣言行には触れない。
    std::fs::write(
        src_dir.join("foo.rs"),
        "pub fn existing_dead() {}\n\npub fn used() {}\n\npub fn new_dead() {}\n",
    )
    .unwrap();

    // --dead-scope touched-symbols (= review --hook デフォルト相当): new_dead だけ残る。
    let out = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--git",
            "--dead-scope",
            "touched-symbols",
        ])
        .output()
        .expect("run dead-code");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        dead.contains(&"new_dead"),
        "今回追加した new_dead は touched-symbols スコープで残るべき: {dead:?}"
    );
    assert!(
        !dead.contains(&"existing_dead"),
        "宣言行が hunk と重ならない existing_dead は touched-symbols スコープから除外されるべき: {dead:?}"
    );

    // --dead-scope all (デフォルト): existing_dead も new_dead も両方残る (used は参照
    // されていないが lib crate なら本来 dead 扱い)。
    let out_all = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--git",
            "--dead-scope",
            "all",
        ])
        .output()
        .expect("run dead-code all");
    let json_all: serde_json::Value = serde_json::from_slice(&out_all.stdout).unwrap();
    let dead_all: Vec<&str> = json_all["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        dead_all.contains(&"existing_dead"),
        "all スコープでは元からあった existing_dead も返るべき: {dead_all:?}"
    );
    assert!(
        dead_all.contains(&"new_dead"),
        "all スコープでも new_dead は返るべき: {dead_all:?}"
    );

    // 退行ガード: dir パスの問題で std::path::Path を使う警告を避ける。
    let _ = Path::new(root);
}

/// `dead_symbols[].file` は常に `/` 区切りで返す。
///
/// このパスは `filter_dead_by_touched_symbols` / `filter_dead_by_wip_added` で
/// unified diff 由来のパス (`DiffFile.new_path` / `ApiSymbol.file`、常に `/` 区切り)
/// と突き合わせるほか、review JSON では `impact.changes[].path` /
/// `missing_cochanges[].file` と同じ文書内に並ぶ。`Path::to_string_lossy` の値を
/// そのまま使うと Windows で `\` 区切りになり、突合せが常に不一致になる。
#[test]
fn dead_code_reports_forward_slash_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::create_dir_all(repo.join("src/nested")).expect("create nested dir");
    std::fs::write(
        repo.join("src/nested/lib.ts"),
        "export function neverUsed(): void {}\n",
    )
    .expect("write lib.ts");

    let out = cargo_bin()
        .args(["dead-code", "--dir", repo.to_str().unwrap()])
        .output()
        .expect("run dead-code");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let files: Vec<&str> = dead.iter().filter_map(|s| s["file"].as_str()).collect();

    assert!(
        files.contains(&"src/nested/lib.ts"),
        "dead_symbols[].file は `/` 区切りの workspace 相対パスであるべき: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains('\\')),
        "dead_symbols[].file にプラットフォーム固有の区切り文字を含めてはいけない: {files:?}"
    );
}

/// `--dead-scope touched-symbols` はネストしたディレクトリのファイルでも機能する。
///
/// `dead_scope_touched_symbols_excludes_pre_existing_dead` の姉妹テスト。
/// 突合せキーであるパスの区切り文字が diff 側と揃っていないと、changed file 判定が
/// 常に外れて dead が 0 件になる (`review --hook` の既定スコープで検出が消える)。
#[test]
fn dead_scope_touched_symbols_matches_nested_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src/deep/nest")).expect("create nested dir");
    std::fs::write(
        root.join("src/deep/nest/mod.ts"),
        "export const KEEP = 1;\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let s = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(s.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight-tests"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // 未参照の export を追加。宣言行が diff hunk に含まれる。
    std::fs::write(
        root.join("src/deep/nest/mod.ts"),
        "export const KEEP = 1;\n\nexport function addedDead(): void {}\n",
    )
    .unwrap();

    let out = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--git",
            "--dead-scope",
            "touched-symbols",
        ])
        .output()
        .expect("run dead-code");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        dead.contains(&"addedDead"),
        "ネストしたパスでも touched-symbols スコープで検出されるべき: {dead:?}"
    );
}

/// dead-code の検出結果には宣言行を含める。
///
/// `file` だけだと利用者が結局シンボルを探し直すことになり、
/// 「識別子検索を AST に置き換える」というツールの目的と噛み合わない。
#[test]
fn dead_code_reports_declaration_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::write(
        repo.join("lib.ts"),
        "export function usedFn() {\n  return 1;\n}\n\nexport function unusedFn() {\n  return 2;\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("main.ts"),
        "import { usedFn } from './lib';\nusedFn();\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", repo.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols");
    let entry = dead
        .iter()
        .find(|d| d["name"] == "unusedFn")
        .unwrap_or_else(|| panic!("unusedFn が dead に出るべき: {json}"));
    assert_eq!(
        entry["line"].as_u64(),
        Some(4),
        "宣言行 (0-indexed) が付くこと: {entry}"
    );
    assert!(
        !dead.iter().any(|d| d["name"] == "usedFn"),
        "参照のある usedFn は dead ではない: {json}"
    );
}

#[test]
fn dead_code_excludes_python_dynamic_protocol_callbacks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::write(
        repo.join("handlers.py"),
        concat!(
            "class UrlCallback(urllib.request.HTTPHandler):\n",
            "    def http_open(self, request):\n",
            "        return request\n",
            "    def helper(self):\n",
            "        return None\n",
            "\n",
            "class WatchCallback(FileSystemEventHandler):\n",
            "    def on_any_event(self, event):\n",
            "        return event\n",
        ),
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", repo.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols");
    assert!(
        !dead
            .iter()
            .any(|entry| entry["name"] == "UrlCallback.http_open"),
        "URL ハンドラの動的メソッドは dead ではない: {json}"
    );
    assert!(
        !dead
            .iter()
            .any(|entry| entry["name"] == "WatchCallback.on_any_event"),
        "ファイル監視の動的メソッドは dead ではない: {json}"
    );
    assert!(
        dead.iter()
            .any(|entry| entry["name"] == "UrlCallback.helper"),
        "規約外の未参照メソッドは dead のままにする: {json}"
    );
}

/// closure パラメータのシャドーイングだけで「参照あり」に見えていた関数が dead になる。
///
/// `refs` が名前一致だけで数えるため、`|(_, tail)| tail` のようなローカル束縛が
/// 同名関数への参照として計上され、本番参照ゼロの関数が live と誤認されていた
/// (dead-code の fail-open)。実際に参照されている関数が dead に出ないこと (対照) も固定する。
#[test]
fn dead_code_rust_closure_shadowing_does_not_keep_symbol_alive() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("lib.rs"),
        "pub fn shadowed_only(path: &str) -> &str {\n\
    path.rsplit_once('/').map_or(path, |(_, shadowed_only)| shadowed_only)\n\
}\n\
\n\
pub fn really_used() -> u8 { 1 }\n\
\n\
pub fn caller() -> u8 { really_used() }\n",
    )
    .unwrap();

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
        dead.iter().any(|n| n == "shadowed_only"),
        "closure 束縛にシャドーイングされているだけの関数は dead: {dead:?}"
    );
    assert!(
        !dead.iter().any(|n| n == "really_used"),
        "実際に呼ばれている関数は dead ではない (対照): {dead:?}"
    );
}

/// 解析できないソース (`.vue` 等) がディレクトリにあると、その中の参照を数えられない。
/// 旧実装はこれを黙って落としていたため、`.vue` の `<script>` からしか使われていない
/// TS 関数が **dead と報告される** (生きているシンボルを dead と誤判定する最悪方向の誤り)
/// のに、利用者は `scanned_files` の値からしか気付けなかった。
///
/// 対照として、解析できないソースが無いディレクトリでは `truncations` を出さない
/// (出力バイト列が従来と同一 = 既存 consumer の契約を壊さない) ことも同時に固定する。
#[test]
fn dead_code_declares_unanalyzable_sources() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("api.ts"),
        "export function usedOnlyFromVue() { return 42; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("App.vue"),
        "<script setup lang=\"ts\">\nimport { usedOnlyFromVue } from './api';\nconsole.log(usedOnlyFromVue());\n</script>\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    let truncations = json["truncations"]
        .as_array()
        .expect("解析できないソースがあるので truncations が出るべき");
    assert_eq!(
        truncations.len(),
        1,
        "拡張子単位に 1 件へ畳む: {truncations:?}"
    );
    assert_eq!(truncations[0]["reason"], "unanalyzable_source");
    let message = truncations[0]["message"].as_str().unwrap();
    assert!(
        message.contains(".vue") && message.contains("App.vue"),
        "拡張子と代表パスを含めること: {message}"
    );

    // 対照: 解析できないソースが無ければ truncations 自体を出さない
    let clean = tempfile::TempDir::new().unwrap();
    std::fs::write(
        clean.path().join("api.ts"),
        "export function trulyDead() { return 0; }\n",
    )
    .unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", clean.path().to_str().unwrap()])
        .output()
        .expect("failed to run");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("truncations").is_none(),
        "解析できないソースが無ければ truncations は出力しない: {json}"
    );
}

/// 解析できないソースの申告は、候補の絞り込み (`--glob` / `--git`) ではなく
/// **参照を数えた範囲** (ディレクトリ全体) に合わせる。
///
/// 旧実装は申告を候補の走査から出していたため、`--glob 'src/**/*.ts'` では `.vue` が
/// glob に掛からず、`--git` では申告用の走査自体が無く、どちらも申告なしで `.vue` からしか
/// 使われていない `helper` を dead と報告していた (参照集計は常に全体を見ているので、
/// 「数えていない場所」が申告から消えていた)。同じ入力で `review --git` は申告していた。
#[test]
fn dead_code_declares_unanalyzable_sources_for_glob_and_git() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("src/helper.ts", "export function helper() { return 42; }\n");
    repo.write(
        "src/App.vue",
        "<script setup lang=\"ts\">\nimport { helper } from './helper';\nconsole.log(helper());\n</script>\n",
    );
    repo.init_git();
    repo.commit_all("init");
    repo.write(
        "src/helper.ts",
        "export function helper() { return 42; }\nexport function helper2() { return 1; }\n",
    );

    for args in [&["--glob", "src/**/*.ts"][..], &["--git"][..]] {
        let json = repo.run_json("dead-code", args);
        let truncations = json["truncations"]
            .as_array()
            .unwrap_or_else(|| panic!("{args:?}: .vue の申告が必要: {json}"));
        assert!(
            truncations
                .iter()
                .any(|t| t["reason"] == "unanalyzable_source"
                    && t["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("src/App.vue"))),
            "{args:?}: {json}"
        );
    }

    // 対照: 解析できないソースが無ければ、絞り込み経路でも truncations を出さない。
    let clean = TestRepo::new();
    clean.create_dir_all("src");
    clean.write("src/api.ts", "export function trulyDead() { return 0; }\n");
    let json = clean.run_json("dead-code", &["--glob", "src/**/*.ts"]);
    assert!(json.get("truncations").is_none(), "{json}");
}

/// 画像やデータファイルは「解析できないソース」ではないので申告しない
/// (全件申告するとノイズになり、本当に見落としているソースが埋もれる)。
#[test]
fn dead_code_does_not_declare_non_source_files() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("api.ts"), "export function f() { return 1; }\n").unwrap();
    std::fs::write(root.join("logo.png"), [0x89, 0x50, 0x4e, 0x47]).unwrap();
    std::fs::write(root.join("data.json"), "{\"a\":1}\n").unwrap();
    std::fs::write(root.join("README.md"), "# doc\n").unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("truncations").is_none(),
        "非ソースファイルは申告対象外: {json}"
    );
}

/// 抑制は利用者の意思表示であり、参照や公開APIの生存証拠ではない。
#[test]
fn dead_code_rust_allow_dead_marker_is_shared_with_review_without_changing_refs() {
    let repo = TestRepo::new();
    repo.init_git();
    std::fs::create_dir(repo.root().join("src")).unwrap();
    repo.write("src/lib.rs", "pub fn kept() {}\npub fn unused() {}\n");
    repo.commit_all("init");
    let before = repo.run_json("refs", &["--name", "kept"]);
    repo.write(
        "src/lib.rs",
        "// astro-sight:allow-dead\npub fn kept() {}\npub fn unused() {}\n",
    );
    let after = repo.run_json("refs", &["--name", "kept"]);
    assert_eq!(
        before["refs"].as_array().unwrap().len(),
        after["refs"].as_array().unwrap().len()
    );
    assert_eq!(after["refs"][0]["kind"], "def");
    let dead = repo.run_json("dead-code", &[]);
    let names: Vec<_> = dead["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"unused") && !names.contains(&"kept"),
        "{dead}"
    );
    let review = repo.run_json("review", &["--git", "--dead-scope", "all"]);
    let names: Vec<_> = review["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"unused") && !names.contains(&"kept"),
        "{review}"
    );
    assert!(
        review["api_changes"]["removed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        review["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        review["api_changes"]["added"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// 公開 union も構造体と同じく dead-code の候補になり、使われていれば候補にしない (Issue #277)。
#[test]
fn dead_code_rust_unused_public_union_is_reported() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod values; pub mod call;\n");
    repo.write(
        "src/values.rs",
        "pub union Unused { pub number: u32 }\npub union Used { pub number: u32 }\n",
    );
    repo.write(
        "src/call.rs",
        "use crate::values::Used; pub fn call() -> u32 { let value = Used { number: 1 }; unsafe { value.number } }\n",
    );
    let result = repo.run_json("dead-code", &[]);
    let dead: Vec<(&str, &str)> = result["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| Some((item["name"].as_str()?, item["kind"].as_str()?)))
        .collect();
    assert!(dead.contains(&("Unused", "union")), "{result}");
    assert!(!dead.iter().any(|(name, _)| *name == "Used"), "{result}");
}
