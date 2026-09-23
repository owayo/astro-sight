//! 差分入力 (git diff の取得・解析) と影響分析の Pass 1 / hook 判定の統合テスト。
//!
//! いずれも「呼び出し側を壊す変更を `impact --hook` / `review --hook` が素通しする」
//! 見逃しの回帰テスト。実 git で diff を作り、実バイナリの exit code と出力で確かめる。

#[allow(unused_imports)]
use super::support::*;

/// `impact --git --hook` (+ 追加引数) を実行し、(成功したか, stderr) を返す。
fn impact_hook(repo: &TestRepo, extra: &[&str]) -> (bool, String) {
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .args(extra)
        .arg("--hook")
        .output()
        .expect("failed to run impact");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `lib.rs` の `helper(a)` を `main.rs` から呼ぶ Rust パッケージをコミットした状態を作る。
fn committed_rust_helper_repo(helper: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", helper);
    repo.write(
        "src/main.rs",
        "use demo::helper;\n\nfn main() {\n    println!(\"{}\", helper(1));\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo
}

/// 利用者の git 設定で `git diff` の出力形式が変わっても、破壊的変更を検出する。
///
/// 旧実装は `a/` / `b/` 接頭辞・色なし・内蔵 diff を前提にしており、どれか 1 つ設定されて
/// いるだけで diff を 1 ファイルも認識できず、`impact --hook` / `review --hook` が exit 0 で
/// 素通ししていた。既定設定のケースを対照として同じループに入れる。
#[test]
fn hooks_detect_breaking_change_regardless_of_user_diff_config() {
    let repo = committed_rust_helper_repo("pub fn helper(a: u32) -> u32 {\n    a + 1\n}\n");
    repo.write(
        "src/lib.rs",
        "pub fn helper(a: u32, b: u32) -> u32 {\n    a + b\n}\n",
    );

    let cases: &[(&str, &[(&str, &str)])] = &[
        ("既定設定 (対照)", &[]),
        ("diff.mnemonicPrefix", &[("diff.mnemonicPrefix", "true")]),
        ("diff.noprefix", &[("diff.noprefix", "true")]),
        ("color.ui=always", &[("color.ui", "always")]),
        ("diff.external", &[("diff.external", "echo")]),
    ];
    for (label, settings) in cases {
        for (key, value) in *settings {
            repo.git(["config", key, value]);
        }
        let (impact_ok, impact_stderr) = impact_hook(&repo, &[]);
        let review = cargo_bin()
            .args([
                "review",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .expect("failed to run review");
        for (key, _) in *settings {
            repo.git(["config", "--unset", key]);
        }
        assert!(
            !impact_ok && impact_stderr.contains("src/main.rs:4"),
            "{label}: impact --hook は呼び出し側 (src/main.rs:4) でブロックすべき: {impact_stderr}"
        );
        assert!(
            !review.status.success(),
            "{label}: review --hook も api.mod でブロックすべき: {}",
            String::from_utf8_lossy(&review.stderr)
        );
    }
}

/// 呼び出し側も同じ diff で追随済みの api.mod は、利用者の git 設定に関係なく
/// `modified_closed_in_diff` (非 blocking) へ降格する。
///
/// 降格判定は呼び出し側ファイルの実変更行を `git diff` から取り直す
/// (`ref_index.rs::changed_new_lines_for_file`)。ここだけ出力形式を固定していなかったため、
/// `diff.mnemonicPrefix` 等があると変更行が空になり、追随済みなのに blocking な `mod` として
/// `review --hook` が exit 1 になっていた。既定設定のケースを対照として同じループに入れる。
#[test]
fn review_hook_closes_updated_callers_regardless_of_user_diff_config() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.ts",
        "export function helper(a: number): number {\n  return a + 1;\n}\n",
    );
    repo.write(
        "src/main.ts",
        "import { helper } from \"./lib\";\nexport function run(): number {\n  return helper(1);\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.ts",
        "export function helper(a: number, b: number): number {\n  return a + b;\n}\n",
    );
    repo.write(
        "src/main.ts",
        "import { helper } from \"./lib\";\nexport function run(): number {\n  return helper(1, 2);\n}\n",
    );

    let cases: &[(&str, &[(&str, &str)])] = &[
        ("既定設定 (対照)", &[]),
        ("diff.mnemonicPrefix", &[("diff.mnemonicPrefix", "true")]),
        ("diff.noprefix", &[("diff.noprefix", "true")]),
        ("color.ui=always", &[("color.ui", "always")]),
    ];
    for (label, settings) in cases {
        for (key, value) in *settings {
            repo.git(["config", key, value]);
        }
        let hook = cargo_bin()
            .args([
                "review",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .expect("failed to run review");
        let review = repo.run_json("review", &["--git"]);
        for (key, _) in *settings {
            repo.git(["config", "--unset", key]);
        }
        assert!(
            hook.status.success(),
            "{label}: 追随済みの api.mod で blocking しない: stdout={} stderr={}",
            String::from_utf8_lossy(&hook.stdout),
            String::from_utf8_lossy(&hook.stderr)
        );
        let names = |bucket: &str| -> Vec<String> {
            review["api_changes"][bucket]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|s| s["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            names("modified_closed_in_diff"),
            vec!["helper".to_string()],
            "{label}: {review}"
        );
        assert!(names("modified").is_empty(), "{label}: {review}");
    }
}

/// 空白を含むファイル名の変更も解析する。git は空白を含むパスのヘッダ行末に TAB を付け
/// (`+++ b/src/my util.ts\t`)、旧実装は TAB ごとパスに取り込んで存在確認で落としていた。
#[test]
fn impact_hook_detects_change_in_file_name_with_spaces() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/my util.ts",
        "export function helper(a: number): number {\n  return a + 1;\n}\n",
    );
    repo.write(
        "src/main.ts",
        "import { helper } from \"./my util\";\nexport function run(): number {\n  return helper(1);\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/my util.ts",
        "export function helper(a: number, b: number): number {\n  return a + b;\n}\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let paths: Vec<&str> = context["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .filter_map(|c| c["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["src/my util.ts"], "{context}");

    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        !ok && stderr.contains("src/main.ts:3"),
        "空白入りパスの変更の呼び出し側でブロックすべき: {stderr}"
    );
}

/// 複数行の引数リストへの引数追加は呼び出し契約の変更。関数名の行が変わらないため、
/// 旧実装は「本体だけの変更」と判定して cross-file 検索から外していた (Rust / TS 両方)。
#[test]
fn impact_hook_detects_parameter_added_to_multi_line_parameter_list() {
    // Rust: 関数名の行は不変で、継続行に `b: u32,` を足す。
    let repo =
        committed_rust_helper_repo("pub fn helper(\n    a: u32,\n) -> u32 {\n    a + 1\n}\n");
    repo.write(
        "src/lib.rs",
        "pub fn helper(\n    a: u32,\n    b: u32,\n) -> u32 {\n    a + b\n}\n",
    );
    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        !ok && stderr.contains("src/main.rs:4"),
        "Rust の複数行ヘッダへの引数追加でブロックすべき: {stderr}"
    );
    // 対照: 同じ関数の本体だけの変更はブロックしない (本体だけの変更を cross-file 検索に
    // 載せない既存の抑制を壊さない)。
    repo.write(
        "src/lib.rs",
        "pub fn helper(\n    a: u32,\n) -> u32 {\n    a + 2\n}\n",
    );
    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(ok, "本体だけの変更はブロックしない: {stderr}");

    // TS: function 宣言と、関数を束縛した const (`export const arrow = (...) => ...`)。
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    let base = "export function helper(\n  a: number,\n): number {\n  return a + 1;\n}\nexport const arrow = (\n  a: number,\n): number => a;\n";
    repo.write("src/util.ts", base);
    repo.write(
        "src/main.ts",
        "import { helper } from \"./util\";\nimport { arrow } from \"./util\";\nexport function run(): number {\n  return helper(1);\n}\nexport function run2(): number {\n  return arrow(1);\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/util.ts",
        "export function helper(\n  a: number,\n  b: number,\n): number {\n  return a + b;\n}\nexport const arrow = (\n  a: number,\n  b: number,\n): number => a;\n",
    );
    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        !ok && stderr.contains("src/main.ts:4") && stderr.contains("src/main.ts:7"),
        "TS の複数行ヘッダへの引数追加 (function / arrow) でブロックすべき: {stderr}"
    );
    // 対照: 本体だけの変更はブロックしない。
    repo.write(
        "src/util.ts",
        "export function helper(\n  a: number,\n): number {\n  return a + 2;\n}\nexport const arrow = (\n  a: number,\n): number => a + 1;\n",
    );
    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(ok, "TS の本体だけの変更はブロックしない: {stderr}");
}

/// 同じファイル内で関数を移動しつつシグネチャを変えた変更を検出する。移動先の hunk は
/// 純追加なので旧実装は `"added"` と判定し、新規シンボルとして cross-file 検索から外していた。
#[test]
fn impact_hook_detects_signature_change_of_function_moved_within_file() {
    let others = "pub fn other() -> u32 {\n    1\n}\n\npub fn third() -> u32 {\n    3\n}\n\npub fn fourth() -> u32 {\n    4\n}\n";
    let repo = committed_rust_helper_repo(&format!(
        "pub fn helper(a: u32) -> u32 {{\n    a + 1\n}}\n\n{others}"
    ));
    repo.write(
        "src/lib.rs",
        format!("{others}\npub fn helper(a: u32, b: u32) -> u32 {{\n    a + b\n}}\n"),
    );
    let context = repo.run_json("context", &["--git"]);
    let affected = &context["changes"][0]["affected_symbols"];
    assert_eq!(
        affected,
        &serde_json::json!([{"name": "helper", "kind": "function", "change_type": "modified"}]),
        "{context}"
    );
    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        !ok && stderr.contains("src/main.rs:4"),
        "移動 + シグネチャ変更の呼び出し側でブロックすべき: {stderr}"
    );

    // 対照: 呼び出し契約を変えない移動 (本体だけ変更) は従来どおり新規扱いでブロックしない。
    repo.write(
        "src/lib.rs",
        format!("{others}\npub fn helper(a: u32) -> u32 {{\n    a + 2\n}}\n"),
    );
    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(ok, "契約を変えない移動はブロックしない: {stderr}");
}

/// 対照: 既存のメソッドの本体をその場で書き換えつつ、同名のオーバーロードを離れた位置に
/// 足した変更は「移動 + シグネチャ変更」ではない (既存の呼び出し側は壊れない)。
#[test]
fn impact_hook_does_not_treat_added_overload_as_moved_method() {
    let others = "\n    public int g() {\n        return 1;\n    }\n\n    public int h() {\n        return 2;\n    }\n\n    public int k() {\n        return 3;\n    }\n";
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/Calc.java",
        format!("public class Calc {{\n    public int f(int a) {{\n        int x = a + 1;\n        return x;\n    }}\n{others}}}\n"),
    );
    repo.write(
        "src/Main.java",
        "public class Main {\n    public static void main(String[] args) {\n        Calc c = new Calc();\n        System.out.println(c.f(1));\n    }\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/Calc.java",
        format!("public class Calc {{\n    public int f(int a) {{\n        int x = a + 2;\n        return x;\n    }}\n{others}\n    public int f(int a, int b) {{\n        return a + b;\n    }}\n}}\n"),
    );

    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        ok,
        "オーバーロードの追加 + 既存メソッドの本体変更はブロックしない: {stderr}"
    );
}

/// `--staged` は index の内容を解析する。hunk の行番号は index を指すため、未ステージの
/// 変更 (先頭への関数追加) がある作業ツリーを読むと別の関数を affected と判定し、
/// 本来の変更の呼び出し側を見逃していた。
#[test]
fn impact_staged_analyzes_index_content_not_worktree() {
    let repo = committed_rust_helper_repo(
        "pub fn first() -> u32 {\n    0\n}\n\npub fn helper(a: u32) -> u32 {\n    a + 1\n}\n",
    );
    let staged = "pub fn first() -> u32 {\n    0\n}\n\npub fn helper(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
    repo.write("src/lib.rs", staged);
    repo.git(["add", "src/lib.rs"]);
    // 未ステージ: 先頭に関数を 2 つ足し、index の hunk と作業ツリーの行をずらす。
    repo.write(
        "src/lib.rs",
        format!(
            "pub fn extra_one() -> u32 {{\n    11\n}}\n\npub fn extra_two() -> u32 {{\n    22\n}}\n\n{staged}"
        ),
    );

    let context = repo.run_json("context", &["--git", "--staged"]);
    let affected: Vec<&str> = context["changes"][0]["affected_symbols"]
        .as_array()
        .expect("affected_symbols")
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert_eq!(
        affected,
        vec!["helper"],
        "index の内容で affected を判定すべき: {context}"
    );
    let (ok, stderr) = impact_hook(&repo, &["--staged"]);
    assert!(
        !ok && stderr.contains("src/main.rs:4"),
        "stage した引数追加の呼び出し側でブロックすべき: {stderr}"
    );
}

/// 変更ファイルと末尾が一致するだけの別ファイル (`packages/app/src/util.ts` と
/// `src/util.ts`) の呼び出し側を、同じファイル内の参照として捨てない。
#[test]
fn impact_hook_keeps_caller_in_file_sharing_path_suffix() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.create_dir_all("packages/app/src");
    repo.write(
        "src/util.ts",
        "export function helper(a: number): number {\n  return a + 1;\n}\nexport function selfUse(): number {\n  return helper(1);\n}\n",
    );
    repo.write(
        "packages/app/src/util.ts",
        "import { helper } from \"../../../src/util\";\nexport function wrap(): number {\n  return helper(1);\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/util.ts",
        "export function helper(a: number, b: number): number {\n  return a + b;\n}\nexport function selfUse(): number {\n  return helper(1);\n}\n",
    );

    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        !ok && stderr.contains("packages/app/src/util.ts:3"),
        "末尾が同じ別ファイルの呼び出し側でブロックすべき: {stderr}"
    );
    // 対照: 変更ファイル自身の中の呼び出しは従来どおり cross-file の caller にしない。
    assert!(
        !stderr.contains("→ src/util.ts"),
        "変更ファイル自身の参照は caller にしない: {stderr}"
    );
}

/// 影響分析の結果に現れない diff 内ファイル (トップレベルの呼び出しだけを更新した
/// スクリプト) の、更新済みの呼び出しでブロックしない。
#[test]
fn impact_hook_accepts_updated_top_level_call_in_diff() {
    let repo = TestRepo::new();
    repo.write("util.py", "def helper(a):\n    return a + 1\n");
    repo.write("script.py", "from util import helper\n\nprint(helper(1))\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write("util.py", "def helper(a, b):\n    return a + b\n");
    repo.write(
        "script.py",
        "from util import helper\n\nprint(helper(1, 2))\n",
    );

    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        ok,
        "同じ diff で更新済みの呼び出しは解決済み (exit 0): {stderr}"
    );

    // 対照: 同じファイルを diff に含めても、呼び出し行を更新していなければブロックする
    // (ファイル単位で解決済みにすると未更新の呼び出しを見逃す)。
    repo.write(
        "script.py",
        "# touched\nfrom util import helper\n\nprint(helper(1))\n",
    );
    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        !ok && stderr.contains("script.py:4"),
        "未更新の呼び出しはブロックすべき: {stderr}"
    );
}

/// Cargo パッケージの `src/bin/` (バイナリターゲットのソース) の呼び出し側を数える。
/// 既定除外の `bin` (ビルド成果物) が任意の階層に一致し、`src/bin/*.rs` の caller が
/// 申告なしで落ちていた (refs / dead-code では数えられる)。
#[test]
fn impact_hook_counts_callers_in_cargo_src_bin() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/bin");
    repo.create_dir_all("bin");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub fn helper(a: u32) -> u32 {\n    a + 1\n}\n",
    );
    repo.write(
        "src/bin/tool.rs",
        "fn main() {\n    println!(\"{}\", demo::helper(1));\n}\n",
    );
    repo.write(
        "bin/gen.rs",
        "fn main() {\n    println!(\"{}\", demo::helper(2));\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        "pub fn helper(a: u32, b: u32) -> u32 {\n    a + b\n}\n",
    );

    let (ok, stderr) = impact_hook(&repo, &[]);
    assert!(
        !ok && stderr.contains("src/bin/tool.rs:2"),
        "Cargo の src/bin の呼び出し側でブロックすべき: {stderr}"
    );
    // 対照: ビルド成果物としての `bin/` は従来どおり除外する。
    assert!(
        !stderr.contains("bin/gen.rs"),
        "src 直下でない bin は除外のまま: {stderr}"
    );
}
