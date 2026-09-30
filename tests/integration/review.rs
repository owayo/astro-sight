//! review サブコマンドと API 差分 (api.add / api.rm / api.mod / api.moved) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn review_hook_distinguishes_independent_names_from_removed_function_calls() {
    let before =
        "def abort(message):\n    raise SystemExit(message)\ndef version():\n    return '1.0'\n";
    let after = "def version():\n    return '1.0'\n";
    for (source, notify, informational) in [
        (
            "def abort(message):\n    print(message)\n",
            "def abort(message):\n    print(message)\n",
            true,
        ),
        (
            "def abort(message):\n    print(message)\nabort('failed')\n",
            "",
            true,
        ),
        ("from tools.util import abort\nabort('failed')\n", "", false),
    ] {
        let output = a3_review_hook(
            |root| std::fs::write(root.join("tools/util.py"), after).unwrap(),
            &[
                ("tools/util.py", before),
                ("tools/report.py", source),
                ("tools/notify.py", notify),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), informational, "{source}\n{stderr}");
        assert_eq!(stderr.contains("\"rm\""), !informational, "{stderr}");
    }
    let before = "pub fn total(x: u32) -> u32 { x }\npub fn keep() -> u32 { 1 }\n";
    let after = "pub fn keep() -> u32 { 1 }\n";
    for (source, informational) in [
        ("pub fn run() -> u32 { let total = 3; total }\n", true),
        (
            "use crate::a::total;\npub fn run() -> u32 { total(3) }\n",
            false,
        ),
    ] {
        let output = a3_review_hook(
            |root| std::fs::write(root.join("src/a.rs"), after).unwrap(),
            &[
                ("src/lib.rs", "pub mod a;\npub mod b;\n"),
                ("src/a.rs", before),
                ("src/b.rs", source),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), informational, "{source}\n{stderr}");
        assert_eq!(stderr.contains("\"rm\""), !informational, "{stderr}");
    }
}

#[test]
fn review_hook_ignores_rust_cfg_keys_when_public_method_is_removed() {
    let manifest = "[package]\nname = \"cfg-key-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n";
    let before = "pub struct Lang;\nimpl Lang { pub fn feature(&self) -> bool { true } }\n";
    let after = "pub struct Lang;\n";
    let flags = "#[cfg(feature = \"extra\")]\npub fn optional() {}\npub fn check() -> bool { cfg!(feature = \"extra\") }\n";

    let cfg_only = a3_review_hook(
        |root| std::fs::write(root.join("src/lib.rs"), after).unwrap(),
        &[
            ("Cargo.toml", manifest),
            ("src/lib.rs", before),
            ("src/flags.rs", flags),
        ],
    );
    assert!(
        cfg_only.status.success(),
        "cfg 条件のキーだけでは削除を blocking にしない: {}",
        String::from_utf8_lossy(&cfg_only.stderr)
    );

    let real_call = a3_review_hook(
        |root| std::fs::write(root.join("src/lib.rs"), after).unwrap(),
        &[
            ("Cargo.toml", manifest),
            ("src/lib.rs", before),
            ("src/flags.rs", flags),
            (
                "src/consumer.rs",
                "use crate::Lang;\npub fn consume(lang: &Lang) { lang.feature(); }\n",
            ),
        ],
    );
    assert!(
        !real_call.status.success(),
        "実際の呼び出しが残る場合は blocking にする: {}",
        String::from_utf8_lossy(&real_call.stderr)
    );
}

#[test]
fn review_python_signature_reformat_preserves_real_contract_changes() {
    let before = "def run(\n    value: str = \"x  y\",\n    timeout: int = 15,\n) -> str:\n    return value\n";
    for (after, compatible) in [
        (
            "def run(value: str = \"x  y\", timeout: int = 15) -> str:\n    return value\n",
            true,
        ),
        (
            "def run( value : str = \"x  y\", # ordinary comment\n timeout : int = 15 , ) -> str:\n    return value\n",
            true,
        ),
        (
            "def run(value: str = \"x y\", timeout: int = 15) -> str:\n    return value\n",
            false,
        ),
        (
            "def run(value: str = \"x  y\", timeout: int = 16) -> str:\n    return value\n",
            false,
        ),
        (
            "def run(value: str = \"x  y\", /, timeout: int = 15) -> str:\n    return value\n",
            false,
        ),
        (
            "def run(*, value: str = \"x  y\", timeout: int = 15) -> str:\n    return value\n",
            false,
        ),
        (
            "def run(value: str = \"x  y\", timeout: int = 15) -> int:\n    return value\n",
            false,
        ),
    ] {
        let output = a3_review_hook(
            |root| std::fs::write(root.join("pkg/api.py"), after).unwrap(),
            &[
                ("pkg/__init__.py", ""),
                ("pkg/api.py", before),
                (
                    "consumer.py",
                    "from pkg.api import run\nprint(run(value=\"a\"))\n",
                ),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), compatible, "{after}\n{stderr}");
        if compatible {
            assert!(!stderr.contains("\"mod\""), "{stderr}");
            assert!(!stderr.contains("\"impacts\""), "{stderr}");
        } else {
            assert!(stderr.contains("\"mod\""), "{stderr}");
        }
    }
}

#[test]
fn review_python_optional_addition_does_not_hide_changed_literal() {
    let before = "def run(value: str = 'a  b') -> str:\n    return value\n";
    for (literal, compatible) in [("'a  b'", true), ("'a b'", false)] {
        let after = format!(
            "def run(\n value: str = {literal},\n timeout: int = 15,\n) -> str:\n    return value\n"
        );
        let output = a3_review_hook(
            |root| std::fs::write(root.join("pkg/api.py"), &after).unwrap(),
            &[
                ("pkg/__init__.py", ""),
                ("pkg/api.py", before),
                ("consumer.py", "from pkg.api import run\nprint(run())\n"),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), compatible, "{after}\n{stderr}");
        assert_eq!(stderr.contains("\"mod_compat\""), compatible, "{stderr}");
        assert_eq!(stderr.contains("\"mod\""), !compatible, "{stderr}");
    }
}

#[test]
fn review_python_root_script_move_no_api_rm() {
    // Issue 2026-06-14-python-script-move-api-rm: root-level の単体スクリプトを package へ移設
    // すると旧スクリプトの top-level helper が api.rm (blocking) で hook を止める FP。
    // script-local として api.rm から除外され hook が clean になることを検証する。
    let script = "def helper():\n    return 1\n\ndef cmd_run(cfg):\n    return helper()\n\ndef main():\n    cmd_run({})\n\nif __name__ == '__main__':\n    main()\n";
    let pyproject = "[project]\nname = \"tool\"\nversion = \"0.1.0\"\n";
    let output = a3_review_hook(
        |root| {
            // build_font.py 削除 + package 化 (untracked), cmd_run の sig 変更
            Command::new("git")
                .args(["rm", "-q", "script.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
            std::fs::create_dir_all(root.join("src/pkg")).unwrap();
            std::fs::write(root.join("src/pkg/__init__.py"), "").unwrap();
            std::fs::write(
                root.join("src/pkg/__main__.py"),
                "from pkg.main import main\nmain()\n",
            )
            .unwrap();
            std::fs::write(
                root.join("src/pkg/main.py"),
                "def helper():\n    return 1\n\ndef cmd_run(ctx):\n    return helper()\n\ndef main():\n    cmd_run(object())\n",
            )
            .unwrap();
        },
        &[("script.py", script), ("pyproject.toml", pyproject)],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // hook は blocking なし → exit 0 (clean)
    assert!(
        output.status.success(),
        "root script move は hook blocking しないべき (exit 0): {stderr}"
    );
    assert!(
        !stderr.contains("\"rm\""),
        "root script の helper は script-local で api.rm にしないべき: {stderr}"
    );
}

#[test]
fn review_python_package_module_removal_keeps_api_rm() {
    // 安全性: package module (サブディレクトリ配下) の関数削除は従来どおり api.rm として残す
    // (A3 が real api.rm を隠さない false negative 回避)。
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "pkg/lib.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("pkg/__init__.py", ""),
            ("pkg/lib.py", "def public_api(x):\n    return x + 1\n"),
            (
                "app.py",
                "from pkg.lib import public_api\nprint(public_api(1))\n",
            ),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // api.rm は blocking → exit 1
    assert!(
        !output.status.success(),
        "package module の api.rm は hook を blocking すべき (exit 1): {stderr}"
    );
    assert!(
        stderr.contains("public_api"),
        "package module の削除は api.rm に残すべき: {stderr}"
    );
}

#[test]
fn review_python_imported_root_module_keeps_api_rm() {
    // 安全性: root-level でも他ファイルから import されているモジュールの削除は api.rm として残す。
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "util.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("util.py", "def helper(x):\n    return x * 2\n"),
            ("app.py", "import util\nprint(util.helper(3))\n"),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "import される root module の api.rm は hook を blocking すべき (exit 1): {stderr}"
    );
    assert!(
        stderr.contains("helper"),
        "import される root module の削除は api.rm に残すべき: {stderr}"
    );
}

#[test]
fn review_python_poetry_script_entry_not_excluded() {
    // 安全性 (codex 指摘): Poetry `[tool.poetry.scripts]` の entrypoint が指す root script を
    // 削除しても script-local として完全除外せず、公開 CLI 面として扱う (api.rm/rm_dead に残す)。
    let pyproject = "[tool.poetry]\nname = \"tool\"\nversion = \"0.1.0\"\n\n[tool.poetry.scripts]\nmytool = \"cli:run\"\n";
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "cli.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("cli.py", "def run():\n    print(\"hi\")\n"),
            ("pyproject.toml", pyproject),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Poetry entrypoint の関数は完全除外されず出力に残る (rm or rm_dead)
    assert!(
        stderr.contains("run"),
        "Poetry script entry の削除は完全除外せず出力に残すべき: {stderr}"
    );
}

#[test]
fn review_python_malformed_pyproject_scripts_fails_closed() {
    // 安全性 (codex 指摘): scripts セクションが存在するが table でない schema 不正な pyproject では
    // 解析不能として fail-closed (script-local 判定を止め、削除を完全除外しない)。
    let pyproject = "[project]\nname = \"tool\"\nscripts = \"cli:run\"\n";
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "cli.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("cli.py", "def run():\n    print(\"hi\")\n"),
            ("pyproject.toml", pyproject),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("run"),
        "schema 不正な pyproject は fail-closed で完全除外しないべき: {stderr}"
    );
}

// ---- review サブコマンドテスト ----

#[test]
fn review_on_clean_repo() {
    // クリーンな状態では変更なしの結果を返すべき
    let output = cargo_bin()
        .args(["review", "--dir", "."])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    // review は JSON 出力を返すこと
    assert!(json.is_object(), "review は JSON オブジェクトを返すべき");
}

#[test]
fn review_xojo_only_diff_returns_empty_result() {
    // lexer-only 言語の review は cross-file 解析も dead-code も skip し、hook の
    // 汎用名ノイズを出さない。Xojo は symbols/dead-code 単体では動くが review では空結果。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    let diff = make_new_file_diff(
        "Main.xojo_code",
        "Class App\nEnd Class\n\nClass Orphan\nEnd Class\n",
    );
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json["impact"]["changes"].as_array().unwrap().is_empty(),
        "Xojo-only review は impact を空にするべき: {json}"
    );
    assert!(
        json["api_changes"]["added"].as_array().unwrap().is_empty()
            && json["api_changes"]["removed"]
                .as_array()
                .unwrap()
                .is_empty()
            && json["api_changes"]["modified"]
                .as_array()
                .unwrap()
                .is_empty(),
        "Xojo-only review は API 差分を空にするべき: {json}"
    );
    assert!(
        json["dead_symbols"].as_array().unwrap().is_empty(),
        "Xojo-only review は dead_symbols を返すべきでない: {json}"
    );
}

#[test]
fn review_respects_framework_laravel_preset() {
    // review が dead-code と同じ `--framework laravel` プリセットを尊重し、
    // app/Http/Controllers 等を dead_symbols から除外することを検証する。
    // 対象プロジェクトのコードは引用せず、Laravel-ish な最小フィクスチャを一次創作する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();

    let controller_src =
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n";
    let service_src =
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n";

    std::fs::write(
        root.join("app/Http/Controllers/SampleController.php"),
        controller_src,
    )
    .unwrap();
    std::fs::write(root.join("app/Services/SampleService.php"), service_src).unwrap();

    let mut diff = String::new();
    diff.push_str(&make_new_file_diff(
        "app/Http/Controllers/SampleController.php",
        controller_src,
    ));
    diff.push_str(&make_new_file_diff(
        "app/Services/SampleService.php",
        service_src,
    ));
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, &diff).unwrap();

    // --framework laravel なし: Controllers/SampleController も dead に出る (回帰担保)
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_without: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        dead_without.iter().any(|n| n.contains("SampleController")),
        "preset なしでは app/Http/Controllers 配下が dead_symbols に残るべき: {dead_without:?}"
    );

    // --framework laravel あり: Controllers/SampleController は除外、Services/SampleService は残る
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_with: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_with.iter().any(|n| n.contains("SampleController")),
        "Laravel preset で app/Http/Controllers 配下は dead_symbols から除外されるべき: {dead_with:?}"
    );
    assert!(
        dead_with
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "app/Services/ は Laravel preset 対象外のため dead 判定が残るべき: {dead_with:?}"
    );
}

#[test]
fn review_hook_suppresses_wip_added_dead_by_default() {
    // `review --hook` の既定: 同一 diff で新規 export されたシンボル (api.added に挙がる)
    // は WIP の純粋ヘルパー追加とみなして dead 警告から除外する
    // (Issue 2026-06-25-wip-dead-symbol-during-incremental-impl)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let src = "export function matchAssigneeName(input: string) {\n    return input;\n}\n";
    std::fs::write(root.join("notes.ts"), src).unwrap();
    let diff = make_new_file_diff("notes.ts", src);
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    // --hook + default (=include_wip_dead 無効): hook exit 0、stdout 無出力
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
            "--hook",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "hook の dead 抑止が効いていれば stdout は空 (= blocking 無し): {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // --hook --include-wip-dead: 抑止解除 ― WIP 新規追加も dead として出る。
    // hook は dead を blocking とみなし stderr に JSON を出して exit != 0 を返す
    // (= caller (Stop hook) に WIP dead を通知)。
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
            "--hook",
            "--include-wip-dead",
        ])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "--include-wip-dead で WIP dead が残れば hook は blocking 検出として exit != 0 を返す"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("matchAssigneeName") && stderr.contains("\"dead\""),
        "--include-wip-dead で抑止を外せば dead が hook の blocking 出力 (stderr) に現れる: {stderr}"
    );
}

#[test]
fn review_without_hook_keeps_wip_added_dead() {
    // `review` 単体 (非 hook): WIP dead 抑止は適用しない。レビュアーが api.added と
    // dead の両者を見て総合判断するため、自動抑止は --hook 経路に限定する設計。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let src = "export function matchAssigneeName(input: string) {\n    return input;\n}\n";
    std::fs::write(root.join("notes.ts"), src).unwrap();
    let diff = make_new_file_diff("notes.ts", src);
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
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
    assert!(
        dead.iter().any(|n| n == "matchAssigneeName"),
        "非 hook の通常 review は WIP dead を抑止せず従来通り全 dead を返す: {dead:?}"
    );
}

#[test]
fn review_framework_unknown_errors() {
    // 未知の framework 値は cmd_dead_code と同じエラー形式で拒否されること。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("dummy.patch"),
        "diff --git a/dummy.php b/dummy.php\nnew file mode 100644\n--- /dev/null\n+++ b/dummy.php\n@@ -0,0 +1,1 @@\n+<?php\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            root.join("dummy.patch").to_str().unwrap(),
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("Unknown framework preset") || combined.contains("INVALID_REQUEST"),
        "エラーメッセージに framework 未対応が示される: {combined}"
    );
}

/// GitLab issue #24 (codex 指摘 1): review --git の API 変更検出 (`api_changes.added`)
/// でも Flyway migration クラスとそのメソッドは出さない。dead-code 経路と整合し、Stop
/// hook が migration 追加のたびに blocking 化しないようにする。
#[test]
fn review_excludes_flyway_java_migration_from_api_added() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("db/migration")).unwrap();
    let migration_src = "package db.migration;\n\
                         import org.flywaydb.core.api.migration.BaseJavaMigration;\n\
                         import org.flywaydb.core.api.migration.Context;\n\
                         public class V1__Init extends BaseJavaMigration {\n\
                             public void migrate(Context context) throws Exception {}\n\
                         }\n";
    std::fs::write(root.join("db/migration/V1__Init.java"), migration_src).unwrap();
    let diff = make_new_file_diff("db/migration/V1__Init.java", migration_src);
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let added_names: Vec<String> = json["api_changes"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !added_names.iter().any(|n| n.contains("V1__Init")),
        "Flyway migration クラスとそのメソッドは API 変更検出にも出さない: {added_names:?}"
    );
}

/// ローカル Issue 2026-06-04-api-rm-false-positive-on-reexport の回帰テスト:
/// ローカル定義を re-export (`export { foo } from "..."`) に置き換えても、利用者から
/// 見た export 面は維持されるため api.rm に出さない。move (同一 diff 内の add) でない
/// 純粋な forwarding でも抑制されることを確認する (b.ts を作らないため reconcile_with_moves
/// では相殺されず、re-export 抑制ロジックが独立して効くことを保証)。
#[test]
fn api_rm_suppressed_for_ts_named_reexport() {
    use std::process::Command;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    let path_str = path.to_str().expect("utf-8");

    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(
        path.join("a.ts"),
        "export function foo() {}\nexport function bar() {}\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    // foo を外部モジュールからの re-export に置換 (b.ts を作らない = move でない)。bar は不変。
    std::fs::write(
        path.join("a.ts"),
        "export { foo } from \"./vendor\";\nexport function bar() {}\n",
    )
    .unwrap();
    git(&["add", "-A"]);

    let output = cargo_bin()
        .args(["review", "--dir", path_str, "--git"])
        .output()
        .expect("run review");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let api_rm: Vec<&str> = json["api"]["rm"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["n"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !api_rm.contains(&"foo"),
        "foo は re-export で forwarding されており api.rm に出すべきでない: api.rm={api_rm:?}"
    );
}

/// ローカル Issue 2026-08-05-moved-name-only-match-and-path-mismatch (パターン B) の回帰テスト:
/// `--dir` がリポジトリルートのサブディレクトリ (2 プロジェクト構成の backend 側など) のとき、
/// `api.moved` の `from` (削除側 = git diff 由来) と `to` (追加側 = 未追跡合成由来) が
/// 別々の基準ディレクトリになり、`to` がリポジトリルートから見て実在しないパスになっていた。
///
/// 不変条件: 同一レコードの `from` / `to` は同じ基準 (`--dir` 相対) で、いずれも `--dir` 配下に
/// 実在すること (`to` は移動先なので working tree に、`from` は base 側に存在する)。
#[test]
fn moved_paths_share_workspace_basis_from_subdirectory() {
    use std::process::Command;
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // ルート直下にも src/ を持つ 2 プロジェクト構成 (frontend + backend/)。
    let app = root.join("backend");
    std::fs::create_dir_all(app.join("src/old_mod")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(root.join("src/frontend.ts"), "export const ui = 1;\n").unwrap();
    std::fs::write(
        app.join("Cargo.toml"),
        "[package]\nname = \"backend\"\n\n[lib]\nname = \"backend\"\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(app.join("src/lib.rs"), "pub mod old_mod;\n").unwrap();
    std::fs::write(app.join("src/old_mod/mod.rs"), "pub mod cache;\n").unwrap();
    std::fs::write(
        app.join("src/old_mod/cache.rs"),
        "pub const DEFAULT_TTL_SECS: u64 = 30;\n\npub fn ttl_for(n: u64) -> u64 {\n    n * DEFAULT_TTL_SECS\n}\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);

    // 旧モジュールを削除し、同名・同シグネチャのシンボルを持つ新モジュールを未追跡で追加する。
    // 新側にだけ `extra_helper` を足して exported symbol 集合をずらすのは、集合が完全一致すると
    // `pair_untracked_renames` が high-confidence rename として先に相殺してしまい
    // `reconcile_with_moves` (= 本テストの対象) まで届かないため。
    std::fs::remove_dir_all(app.join("src/old_mod")).unwrap();
    std::fs::create_dir_all(app.join("src/new_mod")).unwrap();
    std::fs::write(app.join("src/new_mod/mod.rs"), "pub mod store;\n").unwrap();
    std::fs::write(
        app.join("src/new_mod/store.rs"),
        "pub const DEFAULT_TTL_SECS: u64 = 30;\n\npub fn ttl_for(n: u64) -> u64 {\n    n * DEFAULT_TTL_SECS\n}\n\npub fn extra_helper() -> bool {\n    true\n}\n",
    )
    .unwrap();
    std::fs::write(app.join("src/lib.rs"), "pub mod new_mod;\n").unwrap();

    let app_str = app.to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["review", "--dir", app_str, "--git"])
        .output()
        .expect("run review");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let moved = json["api_changes"]["moved"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !moved.is_empty(),
        "同名・同シグネチャのモジュール移動は moved として検出されるべき: {json}"
    );
    for m in &moved {
        let from = m["from"].as_str().expect("from");
        let to = m["to"].as_str().expect("to");
        // 同一基準であること = どちらも --dir 配下の相対パスとして解決できること。
        assert!(
            !from.starts_with("backend/") && !to.starts_with("backend/"),
            "from/to は --dir 相対であるべき (リポジトリルート相対が混ざっている): from={from} to={to}"
        );
        assert!(
            app.join(to).exists(),
            "to は --dir 基準で実在するパスであるべき: to={to}"
        );
        assert!(
            from == "src/old_mod/cache.rs" && to == "src/new_mod/store.rs",
            "from/to は移動元・移動先を指すべき: from={from} to={to}"
        );
    }
    let moved_names: Vec<&str> = moved.iter().filter_map(|m| m["name"].as_str()).collect();
    assert!(
        moved_names.contains(&"DEFAULT_TTL_SECS") && moved_names.contains(&"ttl_for"),
        "同名・同シグネチャのシンボルは moved に集約されるべき: {moved_names:?}"
    );
    // ワークスペース外 (リポジトリルート直下の frontend) は解析対象に含めない。
    assert!(
        !output.stdout.windows(8).any(|w| w == b"frontend"),
        "--dir 配下外のファイルは出力に現れないべき: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// ローカル Issue 2026-06-30-teamspirit-message-map-api-triage の回帰テスト:
/// ローカル型定義を別ファイルへ移動し、元ファイルには from 句なしの
/// `import type { X } ...; export type { X };` を残すリファクタでも、利用者から見た
/// export 面 (import path から取れる名前) は維持されるため api.rm に出さない。
/// 新定義は `keyof MessageMap` で旧定義 (union literal) と signature が異なるため
/// reconcile_with_moves (name+kind+signature 一致) では相殺されず、from 句なし
/// export clause の抑制ロジックが独立して効くことを保証する。
#[test]
fn api_rm_suppressed_for_ts_from_less_type_reexport() {
    use std::process::Command;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    let path_str = path.to_str().expect("utf-8");

    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(
        path.join("messaging.ts"),
        "export type MessageName = \"openTab\" | \"checkTabUrl\";\n\
export function send(name: MessageName) {}\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    // 定義を messages.ts へ移動 (keyof 派生に変更 = 旧 signature と不一致で move 相殺不可)。
    // messaging.ts は import + from 句なし re-export で同名を公開し続ける。
    std::fs::write(
        path.join("messages.ts"),
        "export interface MessageMap {\n\
\topenTab: { body: string };\n\
\tcheckTabUrl: { body: undefined };\n\
}\n\
export type MessageName = keyof MessageMap;\n",
    )
    .unwrap();
    std::fs::write(
        path.join("messaging.ts"),
        "import type { MessageName } from \"./messages\";\n\
\n\
export type { MessageName };\n\
\n\
export function send(name: MessageName) {}\n",
    )
    .unwrap();
    git(&["add", "-A"]);

    let output = cargo_bin()
        .args(["review", "--dir", path_str, "--git"])
        .output()
        .expect("run review");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let api_rm: Vec<&str> = json["api"]["rm"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["n"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !api_rm.contains(&"MessageName"),
        "MessageName は from 句なし export clause で公開が継続しており api.rm に出すべきでない: api.rm={api_rm:?}"
    );
}

#[test]
fn api_changes_rust_module_reexport_detects_modified_signature() {
    // Issue 2026-07-10-rust-module-reexport-api-suppression:
    // private module 内の pub mod を `pub use internal::wifi as wifi_api;` で
    // 再エクスポートしている (valid な) 構成で、公開 pub fn のシグネチャ変更が
    // api.modified (blocking) に出る。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/internal")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"reexport-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod internal;\npub use internal::wifi as wifi_api;\n",
    )
    .unwrap();
    std::fs::write(root.join("src/internal.rs"), "pub mod wifi;\n").unwrap();
    std::fs::write(
        root.join("src/internal/wifi.rs"),
        "pub fn found() -> bool {\n    true\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/internal/wifi.rs"),
        "pub fn found(strict: bool) -> bool {\n    strict\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let modified: Vec<&str> = json["api_changes"]["modified"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        modified.contains(&"found"),
        "module 再エクスポート経由の公開 API シグネチャ変更は api.modified に出るべき: {json}"
    );
}

#[test]
fn api_changes_rust_file_style_module_hierarchy_reexport_detected() {
    // file-style module (`internal.rs`) 内の `pub mod api;` は `internal/api.rs` に
    // 解決される (Rust 2018+)。この階層 (`internal/api.rs` 内の `pub mod v1;` →
    // `internal/api/v1.rs`) を経由した公開 API の変更も検出する
    // (子 module 解決を parent dir 固定にしていた旧実装は取りこぼす)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/internal/api")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"hier-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod internal;\npub use internal::api;\n",
    )
    .unwrap();
    std::fs::write(root.join("src/internal.rs"), "pub mod api;\n").unwrap();
    std::fs::write(root.join("src/internal/api.rs"), "pub mod v1;\n").unwrap();
    std::fs::write(
        root.join("src/internal/api/v1.rs"),
        "pub fn f() -> bool {\n    true\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/internal/api/v1.rs"),
        "pub fn f(strict: bool) -> bool {\n    strict\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let modified: Vec<&str> = json["api_changes"]["modified"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        modified.contains(&"f"),
        "file-style module 階層経由の公開 API 変更も api.modified に出るべき: {json}"
    );
}

#[test]
fn api_changes_rust_private_module_reexport_is_not_public_surface() {
    // `mod wifi;` (private) を `pub use self::wifi as api;` する構成は rustc では
    // E0365 で無効。API 面として扱わない (親の pub_children 条件で弾く)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"e0365-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod wifi;\npub use self::wifi as wifi_api;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/wifi.rs"),
        "pub fn found() -> bool {\n    true\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/wifi.rs"),
        "pub fn found(strict: bool) -> bool {\n    strict\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let modified = json["api_changes"]["modified"].as_array().unwrap();
    assert!(
        modified.is_empty(),
        "E0365 相当の private module 再エクスポートは公開 API 面にしない: {json}"
    );
}

#[test]
fn api_changes_rust_private_module_without_reexport_stays_suppressed() {
    // 対照: 再エクスポートの無い private module の pub fn は API 面でない (回帰確認)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"noexport-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod hidden;\npub fn public_entry() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/hidden.rs"),
        "pub fn secret() -> bool {\n    true\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/hidden.rs"),
        "pub fn secret(strict: bool) -> bool {\n    strict\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let modified = json["api_changes"]["modified"].as_array().unwrap();
    assert!(
        modified.is_empty(),
        "再エクスポートの無い private module の pub fn は api.modified に出ない: {json}"
    );
}

#[test]
fn api_changes_rust_inline_module_hierarchy_reexport_detected() {
    // codex 再レビュー指摘 (重大2): inline module 配下の外部 mod 宣言
    // (`mod internal { pub mod api; }` の api) は `internal/api.rs` に解決される。
    // 基準 dir を inline 階層に追随させないと `api.rs` を誤探索し、
    // その先の公開階層の破壊的 API 変更を取りこぼす。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/internal")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"inlinemod-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod internal {\n    pub mod api;\n}\npub use internal::api;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/internal/api.rs"),
        "pub fn f() -> bool {\n    true\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/internal/api.rs"),
        "pub fn f(strict: bool) -> bool {\n    strict\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let modified: Vec<&str> = json["api_changes"]["modified"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        modified.contains(&"f"),
        "inline module 配下の外部 mod 経由の公開 API 変更も api.modified に出るべき: {json}"
    );
}

#[test]
fn review_hook_ts_shadowed_local_fn_closed_in_diff_exits_zero() {
    // Issue 2026-07-12-api-mod-same-diff-informational の E2E:
    // export 関数のシグネチャ変更 + 対象 caller (複数行呼び出しの引数内のみ変更) 追随済み +
    // 別ファイルに同名ローカル関数、の構成で Stop hook が exit 0 (informational) になる。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/lib")).unwrap();
    std::fs::write(
        root.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    audio: boolean;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/tap.ts"),
        "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({\n        fps: 30,\n        audio: true,\n    });\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    audio: boolean;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({\n        fps: 30,\n        audio: true,\n        cursor: true,\n    });\n}\n",
    )
    .unwrap();

    // JSON 出力でバケットを確認
    let output = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let closed: Vec<&str> = json["api_changes"]["modified_closed_in_diff"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        closed.contains(&"startRecording"),
        "closed-in-diff に降格されるべき: {json}"
    );

    // hook は informational のみなので exit 0
    let hook = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git", "--hook"])
        .output()
        .expect("failed to run");
    assert!(
        hook.status.success(),
        "closed-in-diff のみの diff は hook exit 0: stderr={}",
        String::from_utf8_lossy(&hook.stderr)
    );
}

#[test]
fn review_hook_ts_shadowed_local_fn_unupdated_caller_exits_nonzero() {
    // 対照: 同名ローカル関数があっても、対象 caller が未更新 (diff 外) なら blocking (exit 1)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/lib")).unwrap();
    std::fs::write(
        root.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/tap.ts"),
        "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .unwrap();

    let hook = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git", "--hook"])
        .output()
        .expect("failed to run");
    assert!(
        !hook.status.success(),
        "対象 caller 未更新なら blocking (exit 1) のまま"
    );
}

/// Issue 2026-08-18-cochange-lockfile-without-new-import: 依存追加を繰り返した履歴を持つ
/// リポジトリで、import を 1 行も増減させない本体変更に対して manifest / lock の共変更を
/// 要求しないことを CLI 全体 (通常 JSON 出力) で固定する。
///
/// 修正前は `uv.lock` が engine の候補除外 glob に無く (Cargo.lock 等だけが列挙されていた)、
/// `pyproject.toml` も条件付き相関のまま出ていたため、confidence 100% の警告 2 件が出ていた。
#[test]
fn review_cochange_omits_dependency_manifest_without_import_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    };
    let manifest_of = |deps: &str| format!("[project]\nname = \"demo\"\ndependencies = [{deps}]\n");
    let lock_of = |n: usize| format!("# lock revision {n}\n");
    let app_of = |imports: &str, body: &str| format!("{imports}\n\ndef run():\n{body}\n");
    let helper_of = |n: usize| format!("def helper():\n    return {n}\n");

    let write = |rel: &str, content: &str| {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    };

    write("pyproject.toml", &manifest_of(""));
    write("uv.lock", &lock_of(0));
    write("pkg/app.py", &app_of("", "    return 1"));
    write("pkg/helper.py", &helper_of(0));
    git(&["init", "-q"]);
    git(&["config", "user.email", "a@b.c"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "."]);
    git(&["commit", "-m", "init", "-q"]);

    // 依存追加を 3 回。manifest / lock / app / helper が毎回一緒に変わる履歴を作る。
    let deps = [
        "\"alpha\"",
        "\"alpha\", \"beta\"",
        "\"alpha\", \"beta\", \"gamma\"",
    ];
    let imports = [
        "import alpha",
        "import alpha\nimport beta",
        "import alpha\nimport beta\nimport gamma",
    ];
    for i in 0..3 {
        write("pyproject.toml", &manifest_of(deps[i]));
        write("uv.lock", &lock_of(i + 1));
        write("pkg/app.py", &app_of(imports[i], "    return 1"));
        write("pkg/helper.py", &helper_of(i + 1));
        git(&["add", "."]);
        git(&["commit", "-m", &format!("feat: dep {i}"), "-q"]);
    }

    // 未コミット: app.py の関数本体だけ変更 (import 行の増減なし)。
    write(
        "pkg/app.py",
        &app_of(
            imports[2],
            "    total = 0\n    for i in range(10):\n        total += i\n    return total",
        ),
    );

    let output = cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("run review");
    assert!(output.status.success(), "review should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("review JSON");
    let missing = json["missing_cochanges"]
        .as_array()
        .expect("missing_cochanges array");
    let files: Vec<&str> = missing
        .iter()
        .map(|m| m["file"].as_str().unwrap_or_default())
        .collect();
    assert!(
        !files.contains(&"pyproject.toml"),
        "import 増減の無い本体変更で pyproject.toml を要求してはならない。got: {files:?}"
    );
    assert!(
        !files.contains(&"uv.lock"),
        "import 増減の無い本体変更で uv.lock を要求してはならない。got: {files:?}"
    );
    // 対照: ソース同士の共変更は引き続き出る (除外が広すぎないことの確認)
    assert!(
        files.contains(&"pkg/helper.py"),
        "ソース同士の共変更は検出されるべき。got: {files:?}"
    );
}

/// `review --git --hook` が打ち切り (truncations) を申告すること。
///
/// 未追跡の巨大ファイルは合成 diff に取り込まれない (`MAX_UNTRACKED_FILE_LINES` 超過)。
/// その結果 diff が空になり `emit_review_short_circuit` へ落ちるが、旧実装は
/// `if hook { return Ok(()); }` で truncations ごと捨てていた。**doc コメントの目的と
/// 正反対**で、同じ入力に対し `impact --hook` は `note:` 行で申告するので非対称でもあった。
/// truncation 機能が防ぐはずだった「全部見た」と読める沈黙が hook 経路で再現していた。
///
/// 既存の単体テストは `build_review_hook_json` を直接叩くため `cmd_review` の短絡に
/// 到達せず、この乖離を検出できていなかった。
#[test]
fn review_git_hook_reports_truncations_on_empty_diff() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("seed.ts", "export const seed = 1;\n");
    repo.commit_all("init");

    // 未追跡かつ行数上限 (5,000) 超過 → 合成 diff から除外され truncation が立つ。
    // 他に変更が無いので diff は空になり、短絡経路へ落ちる。
    let huge: String = (0..5_001)
        .map(|i| format!("export const generated{i} = {i};\n"))
        .collect();
    repo.write("generated.ts", huge);

    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .expect("failed to run review --git --hook");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "打ち切りの申告は informational なので exit 0 のままであること: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "--hook は stdout を使わない: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("\"trunc\""),
        "解析対象から外したファイルを hook 出力で申告すること: {stderr}"
    );
    assert!(
        stderr.contains("untracked_file_too_large"),
        "打ち切り理由を添えること: {stderr}"
    );
    assert!(
        stderr.contains("generated.ts"),
        "打ち切ったファイル名を添えること: {stderr}"
    );
    // 対照: 打ち切りは blocking な検出ではないので api / dead は出ない。
    assert!(
        !stderr.contains("\"dead\"") && !stderr.contains("\"api\""),
        "打ち切りの申告だけで blocking 検出を作らないこと: {stderr}"
    );
}

/// 対照: 打ち切りが無い空 diff では `--hook` は従来どおり完全 silent。
///
/// `trunc` を出すようにした副作用で、git 管理外や「変更なし」のたびに hook が
/// 出力してしまう退行を防ぐ。
#[test]
fn review_git_hook_stays_silent_without_truncations() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("seed.ts", "export const seed = 1;\n");
    repo.commit_all("init");

    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .expect("failed to run review --git --hook");

    assert!(output.status.success());
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "打ち切りが無ければ完全 silent: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Issue 2026-08-19-snapshot-directional-cochange: snapshot の期待値だけを更新した差分に対して
/// 生成元テストの変更を要求しないことを CLI 全体 (通常 JSON 出力) で固定する。
///
/// snapshot は被テスト対象の出力が変わったときにも更新されるので、テストと snapshot の履歴相関は
/// 双方向ではない。テストを変えたら snapshot も更新するが、逆は成り立たない。
///
/// 対照を同じ履歴に内蔵する — 実装ファイルは候補に残ること (抑制が広すぎない)、
/// `--include-generated` では従来どおり出ること (解除手段がある) を同時に確認する。
#[test]
fn review_cochange_omits_source_test_for_snapshot_only_update() {
    let repo = TestRepo::new();
    repo.init_git();

    const SNAPSHOT_HEADER: &str = "// Vitest Snapshot v1, https://vitest.dev/guide/snapshot.html";
    let widget_of = |n: usize| format!("export function Widget() {{\n  return {n};\n}}\n");
    let test_of = |n: usize| {
        format!("test('widget {n}', () => {{\n  expect(Widget()).toMatchSnapshot();\n}});\n")
    };
    let snap_of = |n: usize| format!("{SNAPSHOT_HEADER}\n\nexports[`widget {n} 1`] = `{n}`;\n");

    repo.create_dir_all("ui");
    repo.create_dir_all("tests/__snapshots__");

    // 実装・テスト・snapshot が毎回一緒に変わる履歴。
    for i in 0..4 {
        repo.write("ui/widget.tsx", widget_of(i));
        repo.write("tests/widget.test.tsx", test_of(i));
        repo.write("tests/__snapshots__/widget.test.tsx.snap", snap_of(i));
        repo.commit_all(&format!("feat: widget {i}"));
    }

    // 未コミット: snapshot の期待値だけを更新する。
    repo.write("tests/__snapshots__/widget.test.tsx.snap", snap_of(99));

    let root = repo.root().to_str().expect("utf-8 path").to_string();
    let files_from = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["review", "--dir", root.as_str(), "--git"];
        args.extend_from_slice(extra);
        let output = cargo_bin().args(&args).output().expect("run review");
        assert!(output.status.success(), "review should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("review JSON");
        json["missing_cochanges"]
            .as_array()
            .expect("missing_cochanges array")
            .iter()
            .map(|m| m["file"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    let files = files_from(&[]);
    assert!(
        !files.iter().any(|f| f == "tests/widget.test.tsx"),
        "snapshot の更新だけで生成元テストの変更を要求してはならない。got: {files:?}"
    );
    // 対照: 同じ起点でも実装ファイルは候補に残る (抑制が広すぎないことの確認)
    assert!(
        files.iter().any(|f| f == "ui/widget.tsx"),
        "実装ファイルとの共変更は引き続き検出されるべき。got: {files:?}"
    );

    // 対照: 明示的な解除では従来どおり生成元テストも出る
    let inclusive = files_from(&["--include-generated"]);
    assert!(
        inclusive.iter().any(|f| f == "tests/widget.test.tsx"),
        "--include-generated では方向付けを無効化する。got: {inclusive:?}"
    );
}

/// 対応言語でないファイルへの `git mv` (`api.ts` → `api.ts.bak`) は、`review --git --hook` で
/// 旧 API の削除として blocking に報告する。
///
/// 内容同一の rename は hunk を持たないため `parse_unified_diff` に現れず、旧実装は API 差分に
/// 何も渡らないまま exit 0 で通っていた (同じファイルを `git rm` すれば api.rm で止まる)。
/// 対応言語への `git mv` は従来どおり何も報告しない (対照)。
#[test]
fn review_git_hook_reports_rename_to_unsupported_extension_as_removal() {
    let run = |target: &str| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write(
            "src/api.ts",
            "export function fetchUser(id: string): string {\n  return id;\n}\n",
        );
        repo.write(
            "src/app.ts",
            "import { fetchUser } from \"./api\";\n\nexport function main(): string {\n  return fetchUser(\"1\");\n}\n",
        );
        repo.commit_all("init");
        repo.git(["mv", "src/api.ts", target]);
        cargo_bin()
            .args(["review", "--dir"])
            .arg(repo.root())
            .args(["--git", "--hook"])
            .output()
            .expect("failed to run review --git --hook")
    };

    let output = run("src/api.ts.bak");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "blocking にすべき: {stderr}");
    assert!(
        stderr.contains(r#""rm":[{"f":"src/api.ts","n":"fetchUser"}]"#),
        "旧ファイルの API 削除を報告すべき: {stderr}"
    );

    // 対照: 対応言語への rename は内容が同じなら API を変えない。
    let output = run("src/client.ts");
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "対応言語への rename では何も報告しない: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// zsh の代替解析で宣言を失っても、削除の断定と解析範囲の申告を混同しない。
#[test]
fn zsh_parse_errors_are_visible_in_review_refs_and_diagnostics() {
    let repo = TestRepo::new();
    repo.init_git();
    let before = include_str!("../fixtures/bash_parse_recovery/before.zsh");
    let after = include_str!("../fixtures/bash_parse_recovery/after.zsh");
    repo.write("sample.zsh", before);
    repo.write(
        "consumer.zsh",
        "source ./sample.zsh\ncheck_revision 9.5\nload_record\ngone\n",
    );
    repo.commit_all("base");
    repo.write("sample.zsh", after);

    let review = repo.run_json("review", &["--git"]);
    assert_eq!(review["api_changes"]["removed"], serde_json::json!([]));
    assert_eq!(
        review["api_changes"]["uncertain_removals"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(review["truncations"][0]["reason"], "parse_error_region");
    assert!(
        review["truncations"][0]["message"]
            .as_str()
            .unwrap()
            .contains("zsh parsed with Bash grammar")
    );
    let hook = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let hook: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
    assert_eq!(hook["api"]["rm_unverified"].as_array().unwrap().len(), 2);
    assert!(hook["api"].get("rm").is_none() && hook["api"].get("rm_dead").is_none());
    assert_eq!(hook["trunc"][0]["r"], "parse_error_region");

    let refs = repo.run_json(
        "refs",
        &[
            "--name",
            "load_record",
            "--max-results",
            "unlimited",
            "--token-budget",
            "unlimited",
        ],
    );
    let refs = refs["refs"].as_array().unwrap();
    assert!(
        refs.iter()
            .any(|r| r["path"] == "sample.zsh" && r["kind"] == "unknown")
    );
    assert!(
        refs.iter()
            .any(|r| r["path"] == "consumer.zsh" && r["kind"] == "ref")
    );
    // 打ち切り集計にも unknown が残る。
    let limited = repo.run_json("refs", &["--name", "load_record", "--max-results", "1"]);
    assert_eq!(limited["result_summary"]["by_kind"]["unknown"], 1);
    for command in ["symbols", "ast"] {
        for file in ["sample.zsh", "runner"] {
            repo.write(file, after);
            let result = cargo_bin()
                .args([command, "--path"])
                .arg(repo.path(file))
                .output()
                .unwrap();
            assert!(result.status.success());
            let result: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
            assert!(
                result["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|d| d["message"]
                        .as_str()
                        .unwrap()
                        .contains("zsh parsed with Bash grammar")),
                "{result}"
            );
        }
    }
    repo.remove_file("runner");
    // ERROR と同居する実削除は hook を引き続き失敗させる。
    repo.write("sample.zsh", format!("{before}function gone() {{ :; }}\n"));
    repo.commit_all("add removable function");
    repo.write("sample.zsh", after);
    let hook = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert_eq!(hook.status.code(), Some(1));
    let hook: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
    assert!(
        hook["api"]["rm"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["n"] == "gone"),
        "{hook}"
    );
}

/// 解析エラー内の別スコープに同名ヘッダが残っていても、実削除は exit 1 を維持する。
#[test]
fn zsh_parse_errors_keep_real_deletions_blocking_with_nested_lookalikes() {
    let after = include_str!("../fixtures/bash_parse_recovery/after.zsh");
    for opener in [
        "x=$(\n",
        "cat <(\n",
        "cat >(\n",
        "outer() {\n",
        "(\n",
        "$((\n",
        "((\n",
        "[[\n",
        "x=$[\n",
        "echo $'\n",
    ] {
        let source = format!("{opener}{}", after.replace("load_record", "gone"));
        let hook = a3_review_hook(
            |root| std::fs::write(root.join("sample.zsh"), &source).unwrap(),
            &[
                ("sample.zsh", "function gone() { :; }\n"),
                ("consumer.zsh", "source ./sample.zsh\ngone\n"),
            ],
        );
        assert_eq!(
            hook.status.code(),
            Some(1),
            "{opener:?}: {}",
            String::from_utf8_lossy(&hook.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
        assert!(
            json["api"]["rm"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["n"] == "gone"),
            "{json}"
        );
        assert!(json["api"].get("rm_unverified").is_none(), "{json}");
    }
}

/// 別言語の API をシェルへ置き換えた削除は、同名ヘッダが残っても未確認へ降格しない。
#[test]
fn zsh_parse_errors_keep_cross_language_removals_blocking() {
    let shell_before = include_str!("../fixtures/bash_parse_recovery/before.zsh");
    let after = include_str!("../fixtures/bash_parse_recovery/after.zsh");
    for (old_path, new_path, before, is_shell) in [
        (
            "sample.ts",
            "sample.zsh",
            "export function load_record() { return 1; }\n",
            false,
        ),
        (
            "runner",
            "runner",
            "#!/usr/bin/env python3\ndef load_record():\n    return 1\n",
            false,
        ),
        ("runner", "runner", shell_before, true),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(old_path, before);
        if old_path.ends_with(".ts") {
            repo.write(
                "consumer.ts",
                "import { load_record } from './sample';\nload_record();\n",
            );
        } else if is_shell {
            repo.write("consumer.zsh", "load_record\n");
        } else {
            repo.write(
                "consumer.py",
                "from runner import load_record\nload_record()\n",
            );
        }
        repo.commit_all("base");
        if old_path != new_path {
            repo.remove_file(old_path);
        }
        repo.write(new_path, after);
        let diff = format!(
            "diff --git a/{old_path} b/{new_path}\n--- a/{old_path}\n+++ b/{new_path}\n@@ -1,{} +1,{} @@\n{}{}",
            before.lines().count(),
            after.lines().count(),
            before
                .lines()
                .map(|s| format!("-{s}\n"))
                .collect::<String>(),
            after.lines().map(|s| format!("+{s}\n")).collect::<String>(),
        );
        repo.write("change.diff", diff);
        let hook = cargo_bin()
            .args(["review", "--dir"])
            .arg(repo.root())
            .args(["--diff-file"])
            .arg(repo.path("change.diff"))
            .args(["--base", "HEAD", "--hook"])
            .output()
            .unwrap();
        assert_eq!(
            hook.status.code(),
            Some(if is_shell { 0 } else { 1 }),
            "{old_path} shell={is_shell}: {}",
            String::from_utf8_lossy(&hook.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
        if is_shell {
            // 拡張子なしでも、旧 shebang が zsh なら同じ解析不能を未確認として示す。
            assert_eq!(json["api"]["rm_unverified"].as_array().unwrap().len(), 2);
            assert!(json["api"].get("rm").is_none(), "{json}");
        } else {
            assert!(
                json["api"]["rm"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["n"] == "load_record"),
                "{json}"
            );
            assert!(json["api"].get("rm_unverified").is_none(), "{json}");
        }
    }
}
#[test]
fn review_hook_explains_blocking_categories_without_promoting_information() {
    let repo = TestRepo::new();
    repo.write("api.ts", "export function convert(value: string): string { return value; }\nexport const LABEL: string = 'before';\nexport type Old = number;\n");
    repo.write(
        "caller.ts",
        "import { convert, LABEL } from './api';\nconsole.log(convert('a'), LABEL);\n",
    );
    repo.init_git();
    repo.commit_all("base");
    repo.write("api.ts", "export function convert(value: string, mode: string): string { return value; }\nexport const LABEL: string = 'after';\n");
    for (strict, expected) in [
        (false, vec!["impacts", "api.mod"]),
        (true, vec!["impacts", "api.mod", "api.const_value"]),
    ] {
        let mut command = cargo_bin();
        command
            .args(["review", "--dir"])
            .arg(repo.root())
            .args(["--git", "--hook"]);
        if strict {
            command.arg("--strict-public-const-values");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            value["blocking_categories"],
            serde_json::json!(expected),
            "{value}"
        );
        assert!(
            value["api"].get("const_value").is_some() && value["api"].get("rm_dead").is_some(),
            "{value}"
        );
        assert!(
            value.get("dead").is_none(),
            "使用中の公開名は dead にしない: {value}"
        );
    }
}
