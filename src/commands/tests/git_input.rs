//! git diff / blame source 解決と、未追跡ファイル取り込みのテスト。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
#[allow(unused_imports)]
use std::collections::HashSet;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::Cursor;
#[allow(unused_imports)]
use std::process::Command;

// --- git worktree 判定 & 非 git ディレクトリの graceful skip ---

#[test]
fn is_git_work_tree_true_inside_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo_for_test(dir.path());
    assert!(
        is_git_work_tree(dir.path().to_str().expect("utf-8")).expect("rev-parse"),
        "git init 済み dir は worktree 内"
    );
}

#[test]
fn is_git_work_tree_false_outside_repo() {
    // git init しない一時 dir は管理外。
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(
        !is_git_work_tree(dir.path().to_str().expect("utf-8")).expect("rev-parse"),
        "git 管理外 dir は Ok(false)"
    );
}

#[test]
fn resolve_git_diff_skips_non_git_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    match resolve_git_diff(dir.path().to_str().expect("utf-8"), "HEAD", false).expect("resolve") {
        GitDiffInput::Skipped(skip) => {
            assert_eq!(skip.reason.as_str(), "not_git_repository");
            assert_eq!(skip.source.as_str(), "git");
        }
        GitDiffInput::Diff { .. } => panic!("非 git dir では Skipped を返すべき"),
    }
}

#[test]
fn resolve_git_diff_rejects_invalid_base_even_when_non_git() {
    // base 不正は git 管理外でも入力契約違反として弾く (skip より優先)。
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(
        resolve_git_diff(dir.path().to_str().expect("utf-8"), "-x", false).is_err(),
        "先頭 '-' の base は非 git でも Err"
    );
}

/// 行数上限を超える未追跡ファイルは合成 diff に含めず `truncations` に記録する。
///
/// 生成物 (数万行 / 数万 `pub fn`) を合成すると全 exported symbol が api.add 候補になり、
/// `detect_api_changes` の Phase 0 が数万 name を ApiRefIndex に渡して review が数十分かかり、
/// Stop hook が 120 秒でタイムアウトして沈黙する (実測 1.75 秒 → 10 分超)。
/// 除外を silent にすると「全部レビュー済み」と読めるため必ず報告する
/// (Issue 2026-08-04-review-git-untracked-huge-file-blowup)
#[test]
fn resolve_git_diff_excludes_untracked_over_line_limit_and_reports_truncation() {
    use crate::commands::git_input::MAX_UNTRACKED_FILE_LINES;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");

    // 1 行が短いためサイズ上限には達しないが行数上限を超える生成物風ファイル。
    let generated: String = (0..=MAX_UNTRACKED_FILE_LINES)
        .map(|i| format!("pub fn f{i}() {{}}\n"))
        .collect();
    fs::write(repo.join("generated.rs"), &generated).expect("write oversized untracked");
    // 上限内の未追跡は従来どおり合成されること (巻き込みがないこと) も同時に確認する。
    fs::write(repo.join("src/new_helper.rs"), "pub fn helper() {}\n").expect("write small");

    let (diff, truncations) = resolve_git_diff_parts(repo);

    assert!(
        !diff.contains("generated.rs"),
        "行数上限超過の未追跡は合成しない: {diff}"
    );
    assert!(
        diff.contains("+++ b/src/new_helper.rs"),
        "上限内の未追跡は従来どおり合成する: {diff}"
    );
    let reported = truncations
        .iter()
        .find(|t| t.path.as_deref() == Some("generated.rs"))
        .unwrap_or_else(|| panic!("除外した未追跡は truncations に載せるべき: {truncations:?}"));
    assert_eq!(
        reported.reason,
        crate::models::truncation::TruncationReason::UntrackedFileTooLarge
    );
    assert!(
        reported.message.contains("lines"),
        "超過した上限の種類を message に含めるべき: {}",
        reported.message
    );
}

/// サイズ上限を超える未追跡ファイルも同様に除外して報告する (read 前に metadata で弾く)。
#[test]
fn resolve_git_diff_excludes_untracked_over_size_limit_and_reports_truncation() {
    use crate::commands::git_input::MAX_UNTRACKED_FILE_SIZE;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");

    // 行数は少ないがサイズ上限を超えるファイル (長い文字列リテラル 1 本)。
    let padding = "x".repeat(MAX_UNTRACKED_FILE_SIZE);
    fs::write(
        repo.join("blob.rs"),
        format!("pub fn big() -> &'static str {{ \"{padding}\" }}\n"),
    )
    .expect("write oversized untracked");

    let (diff, truncations) = resolve_git_diff_parts(repo);

    assert!(
        !diff.contains("blob.rs"),
        "サイズ上限超過の未追跡は合成しない: {}",
        &diff[..diff.len().min(400)]
    );
    let reported = truncations
        .iter()
        .find(|t| t.path.as_deref() == Some("blob.rs"))
        .unwrap_or_else(|| panic!("除外した未追跡は truncations に載せるべき: {truncations:?}"));
    assert!(
        reported.message.contains("size"),
        "超過した上限の種類を message に含めるべき: {}",
        reported.message
    );
}

/// 上限超過ファイルへの rename は削除 block を除去して api.rm / rm_dead を出さない。
///
/// 上限超過を「無かったこと」にして deleted 側だけ残すと、巨大ファイルを rename した
/// だけで旧ファイルの全 exported symbol が api.rm / rm_dead に流れ込む (実測 5,001 個 /
/// 135KB の hook 出力)。rename 相手としては残し、中身 (Modified diff) だけ合成しないことで
/// 「rename は認識するが中身は見ない」挙動にする
/// (Issue 2026-08-04-review-git-untracked-huge-file-blowup のリグレッション防止)
#[test]
fn resolve_git_diff_oversized_untracked_rename_suppresses_deletion_block() {
    use crate::commands::git_input::MAX_UNTRACKED_FILE_LINES;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 行数上限を超える生成物風ファイルを commit しておく。
    let generated: String = (0..=MAX_UNTRACKED_FILE_LINES)
        .map(|i| format!("pub fn f{i}() {{}}\n"))
        .collect();
    git_commit_files(repo, &[("generated.rs", &generated)], "initial");

    // rename: 削除 + 同内容の未追跡 (git は untracked を diff に出さないので合成対象)。
    fs::rename(repo.join("generated.rs"), repo.join("renamed.rs")).expect("rename");

    let (diff, truncations) = resolve_git_diff_parts(repo);

    assert!(
        !diff.contains("--- a/generated.rs"),
        "rename 元の削除 block は除去すべき (api.rm / rm_dead ノイズを出さない): {}",
        &diff[..diff.len().min(400)]
    );
    assert!(
        !diff.contains("renamed.rs"),
        "上限超過の rename 先は中身を合成しない: {}",
        &diff[..diff.len().min(400)]
    );
    assert!(
        truncations
            .iter()
            .any(|t| t.path.as_deref() == Some("renamed.rs")),
        "中身を見ていないことは報告する: {truncations:?}"
    );

    // 削除 block が残っていない = api.rm / rm_dead が発生しないことを検出側でも確認する。
    let diff_files = crate::engine::diff::parse_unified_diff(&diff);
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.is_empty() && api.removed_dead.is_empty(),
        "上限超過ファイルの rename で削除 API を報告してはいけない: removed={:?} removed_dead={:?}",
        api.removed.len(),
        api.removed_dead.len()
    );
}

/// 上限内の未追跡ファイルだけなら `truncations` は空 (通常時に余計な出力を出さない)。
#[test]
fn resolve_git_diff_reports_no_truncation_for_normal_untracked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");
    fs::write(repo.join("src/new_helper.rs"), "pub fn helper() {}\n").expect("write small");

    let (diff, truncations) = resolve_git_diff_parts(repo);

    assert!(
        diff.contains("+++ b/src/new_helper.rs"),
        "通常サイズの未追跡は合成する: {diff}"
    );
    assert!(
        truncations.is_empty(),
        "上限内なら打ち切りは報告しない: {truncations:?}"
    );
}

/// unstaged (`--git`、非 `--staged`) では未追跡の新規ソースファイルを「全行追加の
/// 新規ファイル」として diff に合成する。git diff は仕様上 untracked を出さないため、
/// これが無いと「同一作業で作成した未追跡 sibling への参照」が未解決影響と誤報される。
/// 非ソース (拡張子で言語判定不可) と .gitignore 対象は合成しない。
#[test]
fn run_git_diff_unstaged_includes_untracked_source_excludes_binary_and_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("src/lib.rs", "pub fn existing() {}\n"),
            (".gitignore", "ignored.rs\n"),
        ],
        "initial",
    );

    // 未追跡: ソース (含む) / 非ソース (除外) / gitignore 対象 (除外)。
    fs::write(repo.join("src/new_helper.rs"), "pub fn helper() {}\n").expect("write untracked src");
    fs::write(repo.join("data.bin"), "binarylike\n").expect("write untracked bin");
    fs::write(repo.join("ignored.rs"), "pub fn ignored() {}\n").expect("write ignored");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");

    assert!(
        diff.contains("+++ b/src/new_helper.rs") && diff.contains("+pub fn helper() {}"),
        "未追跡の新規ソースは全行追加の新規ファイルとして合成されるべき: {diff}"
    );
    assert!(
        !diff.contains("data.bin"),
        "非ソース (言語判定不可) は合成しない: {diff}"
    );
    assert!(
        !diff.contains("ignored.rs"),
        ".gitignore 対象 (--exclude-standard) は合成しない: {diff}"
    );
}

/// `--dir` がリポジトリルートのサブディレクトリのとき、tracked diff と未追跡合成の
/// パス基準が一致することを保証する。
///
/// `git diff` は既定でリポジトリルート相対、`git ls-files --others` は cwd 相対を返すため、
/// `--relative` を付けないと同じ diff の中で 2 つの基準が混ざる。astro-sight の内部規約は
/// 一貫して `dir` 相対 (`Path::new(dir).join(..)` でファイルを読む) なので diff 側を揃える。
/// (Issue 2026-08-05-moved-name-only-match-and-path-mismatch のパターン B)
#[test]
fn run_git_diff_from_subdirectory_uses_workspace_relative_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_subproject_repo_for_test(repo);

    // tracked 変更 (app 配下) + 未追跡追加 (app 配下) + app 外の tracked 変更。
    fs::write(
        repo.join("app/src/kept.rs"),
        "pub fn kept() -> i64 {\n    1\n}\n",
    )
    .expect("modify tracked");
    fs::write(repo.join("app/src/added.rs"), "pub fn added() {}\n").expect("write untracked");
    fs::write(
        repo.join("src/root_side.rs"),
        "pub fn root_side() -> u8 {\n    0\n}\n",
    )
    .expect("modify outside workspace");

    let app_dir = repo.join("app");
    let diff = crate::commands::run_git_diff(app_dir.to_str().expect("utf-8"), "HEAD", false)
        .expect("diff");

    assert!(
        diff.contains("+++ b/src/kept.rs"),
        "tracked diff のパスは --dir 相対であるべき (app/ prefix が付かない): {diff}"
    );
    assert!(
        diff.contains("+++ b/src/added.rs"),
        "未追跡合成のパスも --dir 相対であるべき: {diff}"
    );
    assert!(
        !diff.contains("app/src/"),
        "リポジトリルート相対のパスが混ざってはいけない: {diff}"
    );
    assert!(
        !diff.contains("root_side"),
        "--dir 配下外の変更はワークスペース外なので含めない: {diff}"
    );
}

/// `--dir` = リポジトリルートでは `--relative` は no-op で、従来どおりの出力になる。
/// (サブディレクトリ対応が既存の主運用を変えていないことの固定)
#[test]
fn run_git_diff_at_repo_root_keeps_repository_relative_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_subproject_repo_for_test(repo);

    fs::write(
        repo.join("app/src/kept.rs"),
        "pub fn kept() -> i64 {\n    1\n}\n",
    )
    .expect("modify tracked");
    fs::write(repo.join("app/src/added.rs"), "pub fn added() {}\n").expect("write untracked");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");

    assert!(
        diff.contains("+++ b/app/src/kept.rs"),
        "リポジトリルート実行では従来どおりルート相対: {diff}"
    );
    assert!(
        diff.contains("+++ b/app/src/added.rs"),
        "未追跡合成も従来どおりルート相対: {diff}"
    );
}

/// `git_show_blob` は `dir` 相対パスを受け取る規約なので、サブディレクトリ実行でも
/// そのワークスペース側の blob を引けること。`git show <rev>:<path>` は既定でリポジトリ
/// ルート相対に解決するため、`<rev>:./<path>` 形式で cwd 基準に固定している。
#[test]
fn git_show_blob_resolves_paths_relative_to_workspace_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_subproject_repo_for_test(repo);
    // 同名パスをルート側にも置き、基準を取り違えたら別ファイルを読むようにする。
    git_commit_files(
        repo,
        &[("src/kept.rs", "pub fn root_kept() {}\n")],
        "add root sibling",
    );

    let app_dir = repo.join("app");
    let blob =
        crate::commands::git_show_blob(app_dir.to_str().expect("utf-8"), "HEAD", "src/kept.rs")
            .expect("blob should be readable from subdirectory workspace");
    let text = String::from_utf8(blob).expect("utf-8");
    assert!(
        text.contains("pub fn kept()"),
        "--dir 配下の blob を読むべき (ルート側の同名ファイルではない): {text}"
    );

    let root_blob =
        crate::commands::git_show_blob(repo.to_str().expect("utf-8"), "HEAD", "src/kept.rs")
            .expect("blob should be readable from repo root");
    let root_text = String::from_utf8(root_blob).expect("utf-8");
    assert!(
        root_text.contains("pub fn root_kept()"),
        "リポジトリルート実行では従来どおりルート相対で解決すべき: {root_text}"
    );
}

/// staged モード (`--git --staged`) では未追跡を合成しない (index にある変更のみを尊重)。
#[test]
fn run_git_diff_staged_excludes_untracked_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");
    fs::write(repo.join("src/new_helper.rs"), "pub fn helper() {}\n").expect("write untracked src");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", true).expect("diff");
    assert!(
        !diff.contains("new_helper.rs"),
        "staged モードでは未追跡を合成すべきでない: {diff}"
    );
}

/// untracked 新規ファイルが diff の「解決済み範囲」に入ることを end-to-end で確認する。
/// run_git_diff (unstaged) の出力を parse_unified_diff にかけ、未追跡ファイルが
/// new_path を持つ DiffFile として現れることを検証する (impact 誤検出
/// 2026-06-12-untracked-new-file-impact の回帰防止)。
#[test]
fn impact_includes_untracked_new_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");

    // 既存ファイルの可視性変更 (tracked diff) + 参照を含む未追跡 sibling (report の再現パターン)。
    fs::write(repo.join("src/lib.rs"), "pub(crate) fn existing() {}\n").expect("modify tracked");
    fs::write(
        repo.join("src/provider.rs"),
        "use crate::existing;\npub fn run() { existing(); }\n",
    )
    .expect("write untracked sibling");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");
    let files = crate::engine::diff::parse_unified_diff(&diff);
    assert!(
        files.iter().any(|f| f.new_path == "src/provider.rs"),
        "未追跡 sibling が DiffFile (解決済み範囲) に含まれるべき: {:?}",
        files.iter().map(|f| &f.new_path).collect::<Vec<_>>()
    );
    assert!(
        files.iter().any(|f| f.new_path == "src/lib.rs"),
        "tracked 変更も従来通り含まれるべき"
    );
}

/// 未追跡 rename (A1): tracked file を削除 (unstaged) + 内容同一の untracked sibling を追加
/// すると、high-confidence rename として正規化され api.add / api.rm が出ない。さらに内容同一の
/// 場合は Modified diff を合成しない (hunkless = commit 済みの 100% rename と一致) ので、
/// 削除元・rename 先のどちらも DiffFile に現れず、dead-code / cochange も commit 済みと一致する。
#[test]
fn run_git_diff_untracked_rename_normalizes_and_emits_no_add_or_rm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/mod_a.rs",
            "pub fn foo(name: &str) -> String {\n    name.to_uppercase()\n}\n",
        )],
        "initial",
    );

    // mod_a.rs を削除 (unstaged) + 内容同一の mod_b.rs を untracked 追加 = rename。
    fs::remove_file(repo.join("src/mod_a.rs")).expect("remove old");
    fs::write(
        repo.join("src/mod_b.rs"),
        "pub fn foo(name: &str) -> String {\n    name.to_uppercase()\n}\n",
    )
    .expect("write new");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");
    let diff_files = crate::engine::diff::parse_unified_diff(&diff);

    let api = detect_api_changes(repo.to_str().expect("utf-8"), "HEAD", &diff_files);
    assert!(
        api.added.is_empty(),
        "rename で api.add は出ない: {:?}",
        api.added
    );
    assert!(
        api.removed.is_empty(),
        "rename で api.rm は出ない: {:?}",
        api.removed
    );

    // 内容同一 rename は Modified diff を合成しない → rename 先も削除元も DiffFile に出ない
    // (commit 済みの hunkless 100% rename と一致し、dead-code / cochange の乖離を防ぐ)。
    assert!(
        !diff_files.iter().any(|f| f.new_path == "src/mod_b.rs"),
        "内容同一 rename 先は DiffFile に出ない: {:?}",
        diff_files.iter().map(|f| &f.new_path).collect::<Vec<_>>()
    );
    assert!(
        !diff_files.iter().any(|f| f.old_path == "src/mod_a.rs"),
        "rename 元の Deleted block は除去される: {:?}",
        diff_files.iter().map(|f| &f.old_path).collect::<Vec<_>>()
    );
}

/// 未追跡の symlink は合成しない (パス境界)。symlink_metadata でリンク自身を見て
/// regular file 以外を除外するため、外部のソースファイルを指す symlink でも内容が
/// 合成 diff に漏れない (codex レビュー指摘のセキュリティ境界)。
#[cfg(unix)]
#[test]
fn run_git_diff_unstaged_skips_untracked_symlink() {
    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("secret.rs");
    fs::write(&target, "pub fn secret() {}\n").expect("write external target");

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");
    // 外部のソースファイルを指す未追跡 symlink (ソース拡張子)。
    std::os::unix::fs::symlink(&target, repo.join("link.rs")).expect("symlink");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");
    assert!(
        !diff.contains("link.rs") && !diff.contains("pub fn secret"),
        "未追跡 symlink (外部ソースを指す) は合成すべきでない: {diff}"
    );
}

#[test]
fn resolve_blame_source_files_skips_non_git_without_explicit_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    match resolve_blame_source_files(
        dir.path().to_str().expect("utf-8"),
        true,
        None,
        None,
        None,
        &[],
    )
    .expect("resolve")
    {
        BlameSourceResolution::Skipped(skip) => {
            assert_eq!(skip.reason.as_str(), "not_git_repository");
        }
        BlameSourceResolution::Files(f) => panic!("非 git + 明示 paths 無しは Skipped: {f:?}"),
    }
}

#[test]
fn resolve_blame_source_files_keeps_explicit_paths_when_non_git() {
    // 管理外でも --paths 明示があれば skip せず明示分を返す (明示優先)。
    let dir = tempfile::tempdir().expect("tempdir");
    match resolve_blame_source_files(
        dir.path().to_str().expect("utf-8"),
        true,
        None,
        Some("a.rs,b.rs"),
        None,
        &[],
    )
    .expect("resolve")
    {
        BlameSourceResolution::Files(f) => {
            assert!(f.contains(&"a.rs".to_string()));
            assert!(f.contains(&"b.rs".to_string()));
        }
        BlameSourceResolution::Skipped(_) => panic!("明示 paths があれば skip しない"),
    }
}

#[test]
fn resolve_blame_source_files_filters_default_excludes_for_git_only() {
    // --git 経由で diff から起点ファイルを取得する場合、
    // BLAME_DEFAULT_EXCLUDE_GLOBS に該当する生成物 (dist/, *.lock) は除外される。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(repo, &[("foo.txt", "v1")], "initial");
    git_commit_files(
        repo,
        &[
            ("foo.txt", "v2"),
            ("dist/main.js", "minified"),
            ("Cargo.lock", "lockfile"),
            ("Angular/www/dist/bundle.js", "minified"),
        ],
        "next",
    );

    let BlameSourceResolution::Files(result) = resolve_blame_source_files(
        repo.to_str().expect("utf-8 path"),
        true,
        Some("HEAD~1"),
        None,
        None,
        &[],
    )
    .expect("resolve") else {
        panic!("expected Files");
    };

    assert!(result.contains(&"foo.txt".to_string()), "got: {result:?}");
    assert!(
        !result.iter().any(|p| p == "dist/main.js"),
        "dist/main.js は BLAME_DEFAULT_EXCLUDE_GLOBS で除外されるはず。got: {result:?}"
    );
    assert!(
        !result.iter().any(|p| p == "Cargo.lock"),
        "Cargo.lock は除外されるはず。got: {result:?}"
    );
    assert!(
        !result.iter().any(|p| p == "Angular/www/dist/bundle.js"),
        "サブディレクトリの dist/ も除外されるはず。got: {result:?}"
    );
}

#[test]
fn resolve_blame_source_files_keeps_explicit_paths_unfiltered() {
    // --paths で明示指定した起点はユーザー意図を尊重し、
    // BLAME_DEFAULT_EXCLUDE_GLOBS 該当でも除外しない。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("dummy.txt", "x")], "initial");

    let BlameSourceResolution::Files(result) = resolve_blame_source_files(
        repo.to_str().expect("utf-8 path"),
        false,
        None,
        Some("dist/main.js,Cargo.lock"),
        None,
        &[],
    )
    .expect("resolve") else {
        panic!("expected Files");
    };

    assert!(result.contains(&"dist/main.js".to_string()));
    assert!(result.contains(&"Cargo.lock".to_string()));
}

#[test]
fn resolve_blame_source_files_applies_user_exclude_glob_for_git() {
    // --git 経由のとき --exclude-glob (user_exclude_globs) も適用される。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(repo, &[("foo.txt", "v1")], "initial");
    git_commit_files(
        repo,
        &[
            ("foo.txt", "v2"),
            ("legacy/keep.rs", "old"),
            ("generated/codegen.rs", "auto"),
        ],
        "next",
    );

    let BlameSourceResolution::Files(result) = resolve_blame_source_files(
        repo.to_str().expect("utf-8 path"),
        true,
        Some("HEAD~1"),
        None,
        None,
        &["generated/**".to_string()],
    )
    .expect("resolve") else {
        panic!("expected Files");
    };

    assert!(result.contains(&"foo.txt".to_string()));
    assert!(result.contains(&"legacy/keep.rs".to_string()));
    assert!(
        !result.iter().any(|p| p == "generated/codegen.rs"),
        "ユーザー指定 --exclude-glob は --git 経由の起点に適用される。got: {result:?}"
    );
}

/// `ChangedFileSet` の caller 照合を、相対/絶対パス・canonicalize 失敗の各ケースで固定する。
/// canonicalize 成功時は canonical 集合、失敗時は文字列 fallback 集合だけを見る分岐を検証する。
mod changed_file_set {
    use crate::commands::ChangedFileSet;
    use std::fs;

    #[test]
    fn relative_path_matches_via_canonical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().expect("utf-8 path");
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write a");
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").expect("write b");

        // 相対パスで構築 (dir 基準で絶対化 + canonicalize されて canonical 集合に入る)。
        let set = ChangedFileSet::build(dir_str, ["a.rs"]);
        assert!(
            set.contains_caller(dir_str, "a.rs"),
            "同一相対 caller は一致"
        );
        assert!(
            !set.contains_caller(dir_str, "b.rs"),
            "集合に無い既存ファイルは不一致"
        );
    }

    #[test]
    fn absolute_path_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().expect("utf-8 path");
        let abs = dir.path().join("a.rs");
        fs::write(&abs, "fn a() {}\n").expect("write a");
        let abs_str = abs.to_str().expect("utf-8 path");

        // 絶対パスで構築。絶対 caller も、同一ファイルに解決される相対 caller も
        // canonical 集合経由で一致する。
        let set = ChangedFileSet::build(dir_str, [abs_str]);
        assert!(set.contains_caller(dir_str, abs_str), "絶対 caller は一致");
        assert!(
            set.contains_caller(dir_str, "a.rs"),
            "同一ファイルに解決される相対 caller も canonical 経由で一致"
        );
    }

    #[test]
    fn nonexistent_path_falls_back_to_string_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().expect("utf-8 path");

        // canonicalize 失敗 (存在しないファイル) は文字列 fallback 集合で照合する。
        let set = ChangedFileSet::build(dir_str, ["ghost.rs"]);
        assert!(
            set.contains_caller(dir_str, "ghost.rs"),
            "存在しないファイルは文字列 fallback で一致"
        );
        assert!(
            !set.contains_caller(dir_str, "other_ghost.rs"),
            "別の存在しないファイルは不一致"
        );
    }
}
