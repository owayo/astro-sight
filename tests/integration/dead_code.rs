//! dead-code 検出の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

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
