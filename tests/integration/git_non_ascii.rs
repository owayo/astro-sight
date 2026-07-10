use super::support::TestRepo;

// ---------------------------------------------------------------------------
// 非 ASCII ファイル名の git diff (core.quotepath) 回帰テスト
// ---------------------------------------------------------------------------

/// 非 ASCII ファイル名は core.quotepath (既定 true) だと diff ヘッダが
/// `--- "a/\346..."` の 8 進クォートになり parse_unified_diff が認識できず、
/// changes からファイルごと脱落していた。`run_git_diff` が `-c core.quotepath=off`
/// を強制することで、repo 側で quotepath=true を明示しても生 UTF-8 パスで
/// 解析されることを検証する。
#[test]
fn context_git_diff_non_ascii_filename_is_parsed() {
    let repo = TestRepo::new();
    repo.write(
        "日本語ファイル.ts",
        "export function greet(name: string): string {\n    return name;\n}\n",
    );
    repo.init_git();
    // 既定値やユーザ環境に依存せず、quotepath クォートが起きる条件を明示的に再現する
    // (astro-sight 側の `-c core.quotepath=off` が repo config に勝つことの検証)。
    repo.git(["config", "core.quotepath", "true"]);
    repo.commit_all("initial");

    // シグネチャ変更 (uncommitted)
    repo.write(
        "日本語ファイル.ts",
        "export function greet(name: string, suffix: string): string {\n    return name + suffix;\n}\n",
    );

    let json = repo.run_json("context", &["--git"]);
    let changes = json["changes"].as_array().unwrap();
    let jp_change = changes
        .iter()
        .find(|c| {
            c["path"]
                .as_str()
                .is_some_and(|p| p.contains("日本語ファイル.ts"))
        })
        .unwrap_or_else(|| {
            panic!("非 ASCII 名ファイルの change が changes に出るべき (quotepath 回帰): {json}")
        });
    let affected = jp_change["affected_symbols"].as_array().unwrap();
    assert!(
        affected.iter().any(|s| s["name"].as_str() == Some("greet")),
        "非 ASCII 名ファイルのシグネチャ変更は affected_symbols に出るべき: {json}"
    );
}

/// quotepath クォートで `--- "a/日本語..."` を認識できないと、直後の `+++ /dev/null` が
/// 直前ファイル (ascii.ts) の new_path を上書きし、ascii.ts が削除ファイル扱いになって
/// 公開関数が api.removed へ誤流入していた。削除された非 ASCII ファイルが自分の
/// DiffFile として解析され、ascii.ts は通常の変更ファイルのまま残ることを検証する。
#[test]
fn context_git_diff_non_ascii_deleted_file_does_not_pollute_ascii_file() {
    let repo = TestRepo::new();
    repo.write(
        "ascii.ts",
        "export function keepMe(value: number): number {\n    return value + 1;\n}\n\nexport function callKeep(): number {\n    return keepMe(1);\n}\n",
    );
    repo.write(
        "日本語削除.ts",
        "export function jpOnly(): number {\n    return 42;\n}\n",
    );
    repo.init_git();
    repo.git(["config", "core.quotepath", "true"]);
    repo.commit_all("initial");

    // ascii.ts を軽微修正し、非 ASCII 名ファイルは fs 削除 (unstaged のまま)。
    // git diff の出力順は ascii.ts → 日本語削除.ts で、後者の `+++ /dev/null` が
    // 前者へ誤帰属するのが旧バグの再現条件。
    repo.write(
        "ascii.ts",
        "export function keepMe(value: number, extra: number): number {\n    return value + extra;\n}\n\nexport function callKeep(): number {\n    return keepMe(1, 2);\n}\n",
    );
    repo.remove_file("日本語削除.ts");

    let json = repo.run_json("review", &["--git"]);

    // ascii.ts は削除ファイル (new_path=/dev/null) と誤認されず、通常の変更として
    // impact.changes に affected_symbols 付きで残る。
    let changes = json["impact"]["changes"].as_array().unwrap();
    let ascii_change = changes
        .iter()
        .find(|c| c["path"].as_str() == Some("ascii.ts"))
        .unwrap_or_else(|| {
            panic!(
                "ascii.ts は変更ファイルとして impact.changes に出るべき (削除誤認の回帰): {json}"
            )
        });
    assert!(
        ascii_change["affected_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"].as_str() == Some("keepMe")),
        "ascii.ts のシグネチャ変更は affected_symbols に出るべき: {json}"
    );

    // ascii.ts の関数が api.removed / removed_dead に現れないこと。
    let removed_entries: Vec<(&str, &str)> = json["api_changes"]["removed"]
        .as_array()
        .unwrap()
        .iter()
        .chain(
            json["api_changes"]
                .get("removed_dead")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten(),
        )
        .filter_map(|s| Some((s["name"].as_str()?, s["file"].as_str()?)))
        .collect();
    assert!(
        !removed_entries.iter().any(|(_, file)| *file == "ascii.ts"),
        "ascii.ts は削除されていないので api.removed に出るべきでない: {removed_entries:?}"
    );
    // 削除された非 ASCII ファイル自身の関数は正しく削除側として計上される (正の対照)。
    assert!(
        removed_entries
            .iter()
            .any(|(name, file)| *name == "jpOnly" && file.contains("日本語削除.ts")),
        "削除された非 ASCII ファイルの jpOnly は removed/removed_dead に出るべき: {removed_entries:?}"
    );
}
