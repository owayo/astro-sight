//! 共変更検出 (`detect_missing_cochanges`) と CLI フラグ等価性のテスト。

#[allow(unused_imports)]
use crate::commands::review::missing_cochange::*;
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

// ------------------------------------------------------------------
// is_dependency_manifest_pair
// ------------------------------------------------------------------

#[test]
fn is_dependency_manifest_pair_matches_cargo() {
    assert!(is_dependency_manifest_pair("Cargo.toml", "Cargo.lock"));
    assert!(is_dependency_manifest_pair("Cargo.lock", "Cargo.toml"));
}

#[test]
fn is_dependency_manifest_pair_matches_node_lockfiles() {
    for lock in ["package-lock.json", "pnpm-lock.yaml", "yarn.lock"] {
        assert!(
            is_dependency_manifest_pair("package.json", lock),
            "package.json ↔ {lock} should match"
        );
    }
}

#[test]
fn is_dependency_manifest_pair_matches_other_ecosystems() {
    let pairs = [
        ("pyproject.toml", "uv.lock"),
        ("pyproject.toml", "poetry.lock"),
        ("pyproject.toml", "pdm.lock"),
        ("Gemfile", "Gemfile.lock"),
        ("composer.json", "composer.lock"),
        ("go.mod", "go.sum"),
        ("mix.exs", "mix.lock"),
    ];
    for (a, b) in pairs {
        assert!(is_dependency_manifest_pair(a, b), "{a} ↔ {b} should match");
    }
}

#[test]
fn is_dependency_manifest_pair_rejects_unrelated_files() {
    assert!(!is_dependency_manifest_pair("src/lib.rs", "Cargo.toml"));
    assert!(!is_dependency_manifest_pair("Cargo.toml", "README.md"));
    assert!(!is_dependency_manifest_pair(
        "package.json",
        "tsconfig.json"
    ));
}

#[test]
fn is_dependency_manifest_pair_rejects_cross_directory_pairs() {
    // monorepo: 異なるディレクトリのマニフェスト/ロックは別プロジェクトなので除外対象外
    assert!(!is_dependency_manifest_pair(
        "apps/web/package.json",
        "apps/api/package-lock.json"
    ));
    assert!(!is_dependency_manifest_pair(
        "crates/foo/Cargo.toml",
        "crates/bar/Cargo.lock"
    ));
}

#[test]
fn is_dependency_manifest_pair_accepts_same_directory_pairs() {
    assert!(is_dependency_manifest_pair(
        "apps/web/package.json",
        "apps/web/package-lock.json"
    ));
    assert!(is_dependency_manifest_pair(
        "crates/foo/Cargo.toml",
        "crates/foo/Cargo.lock"
    ));
}

// ------------------------------------------------------------------
// detect_missing_cochanges: 依存マニフェスト/ロックペアを除外する
// ------------------------------------------------------------------

/// Cargo.toml ↔ Cargo.lock が過去繰り返し共変更されていても
/// Cargo.lock のみの変更で missing_cochange 警告を出さない。
#[test]
fn detect_missing_cochanges_excludes_cargo_manifest_lock_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // Cargo.toml と Cargo.lock を何度も共変更（cochange 統計を作る）
    for i in 0..4 {
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", &format!("# v{i}\n")),
                ("Cargo.lock", &format!("# lock v{i}\n")),
            ],
            &format!("dep update {i}"),
        );
    }

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    // Cargo.lock のみが変更された状況（cargo update -p 相当）
    changed_files.insert("Cargo.lock".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.3,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().all(|m| m.file != "Cargo.toml"),
        "Cargo.toml が missing_cochange に含まれてはならない。got: {missing:?}"
    );
}

#[test]
fn detect_missing_cochanges_uses_review_base_for_multi_commit_ranges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[
            (
                "a.rs",
                "fn a() {\n    let first = 0;\n    let second = 0;\n}\n",
            ),
            (
                "b.rs",
                "fn b() {\n    let first = 0;\n    let second = 0;\n}\n",
            ),
        ],
        "initial",
    );
    git_commit_files(
        repo,
        &[
            (
                "a.rs",
                "fn a() {\n    let first = 1;\n    let second = 0;\n}\n",
            ),
            (
                "b.rs",
                "fn b() {\n    let first = 1;\n    let second = 0;\n}\n",
            ),
        ],
        "pair 1",
    );
    git_commit_files(
        repo,
        &[
            (
                "a.rs",
                "fn a() {\n    let first = 1;\n    let second = 2;\n}\n",
            ),
            (
                "b.rs",
                "fn b() {\n    let first = 1;\n    let second = 2;\n}\n",
            ),
        ],
        "pair 2",
    );
    git_commit_files(
        repo,
        &[(
            "a.rs",
            "fn a() {\n    let first = 10;\n    let second = 2;\n}\n",
        )],
        "a only 1",
    );
    git_commit_files(
        repo,
        &[(
            "a.rs",
            "fn a() {\n    let first = 10;\n    let second = 20;\n}\n",
        )],
        "a only 2",
    );

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("a.rs".to_string());

    // 小サンプル (co=2, denom=2) なので新デフォルト β=8 では
    // score=(2+1)/(2+1+8)=0.27 となり、production の min_confidence=0.3
    // からは弾かれる。本テストは「base が blame 解析に正しく渡る」を
    // 確かめるのが目的なので、閾値を 0.0 に下げて信号の有無だけ見る。
    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        Some("HEAD~2"),
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().any(|m| m.file == "b.rs"),
        "review の base が blame 解析に渡らず HEAD~1 のみを見ると b.rs を見落とす。got: {missing:?}"
    );
}

/// review の detect_missing_cochanges が cochange 入力検証エラーを silent に握り潰さず
/// 呼び出し側へ伝播することを確認する回帰テスト。
#[test]
fn detect_missing_cochanges_propagates_invalid_request_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("a.rs", "v1")], "initial");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("a.rs".to_string());

    // NaN は AppService::analyze_cochange の入力検証で InvalidRequest を返すため、
    // detect_missing_cochanges もそのエラーを伝播するはず。
    let result = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        f64::NAN,
        None,
    );

    let err = result.expect_err("NaN min_confidence should surface as error");
    let astro_err = err
        .downcast_ref::<crate::error::AstroError>()
        .expect("expect AstroError");
    assert_eq!(astro_err.code, crate::error::ErrorCode::InvalidRequest);
}

/// 起点ファイル数が `max_source_files` を超えても review 経路は InvalidRequest で
/// 落ちず、cochange フェーズだけを skip して `SourceFilesExceedLimit` 診断付きの
/// 空レポートを返す回帰テスト。退化した作業ツリー (no-checkout worktree 等) で
/// diff が全追跡ファイルに化けたとき、review には上限を制御するフラグが無いため
/// analyze_cochange のガードをそのまま伝播すると review 全体が exit 1 になる。
#[test]
fn detect_missing_cochanges_skips_cochange_when_sources_exceed_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("a.rs", "v1")], "initial");

    let service = AppService::new();
    // ガードは analyze_cochange 呼び出し前に効くため、実ファイルは不要。
    let limit = crate::models::cochange::CoChangeOptions::default().max_source_files;
    assert!(limit > 0, "既定の max_source_files は正の値のはず");
    let changed_files: HashSet<String> = (0..=limit).map(|i| format!("src/file_{i}.rs")).collect();

    let report = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.3,
        None,
    )
    .expect("exceeding max_source_files must not fail the review pipeline");

    assert!(
        report.missing.is_empty(),
        "cochange 解析を skip したので missing は空のはず。got: {:?}",
        report.missing
    );
    assert!(
        report
            .diagnostics
            .reasons
            .contains(&crate::models::cochange::CoChangeDiagnosticReason::SourceFilesExceedLimit),
        "skip 理由が diagnostics.reasons に載るはず。got: {:?}",
        report.diagnostics.reasons
    );
    assert_eq!(report.diagnostics.sources_requested, changed_files.len());
}

/// `cochange` の `--ignore-merges` / `--include-merges` の CLI パーサ挙動と、
/// dispatch 側の `resolved_ignore_merges = !include_merges` への等価簡約を固定する。
/// (main.rs `dispatch_cochange` はバイナリクレート側でテストから直接呼べないため、
/// パーサ結果と旧 3 分岐ロジックの一致で等価性を担保する)
mod cochange_ignore_merges {
    use crate::cli::{Cli, Commands};
    use crate::models::cochange::CoChangeOptions;
    use clap::Parser;

    /// パース結果から `(ignore_merges, include_merges)` を取り出す。
    fn parse_flags(args: &[&str]) -> (bool, bool) {
        let cli = Cli::try_parse_from(args).expect("cochange args should parse");
        match cli.command {
            Commands::Cochange {
                ignore_merges,
                include_merges,
                ..
            } => (ignore_merges, include_merges),
            other => panic!("expected Cochange, got {other:?}"),
        }
    }

    /// 旧 3 分岐ロジック (等価性判定の基準)。
    fn legacy_resolved(ignore_merges: bool, include_merges: bool) -> bool {
        let defaults = CoChangeOptions::default();
        if include_merges {
            false
        } else if ignore_merges {
            true
        } else {
            defaults.ignore_merges
        }
    }

    #[test]
    fn default_resolves_ignore_merges_true() {
        let (ignore_merges, include_merges) = parse_flags(&["astro-sight", "cochange"]);
        assert!(!ignore_merges);
        assert!(!include_merges);
        let resolved = !include_merges;
        assert_eq!(resolved, legacy_resolved(ignore_merges, include_merges));
        assert!(resolved, "既定は merge 除外 (ignore_merges=true)");
    }

    #[test]
    fn ignore_merges_flag_resolves_true() {
        let (ignore_merges, include_merges) =
            parse_flags(&["astro-sight", "cochange", "--ignore-merges"]);
        assert!(ignore_merges);
        assert!(!include_merges);
        let resolved = !include_merges;
        assert_eq!(resolved, legacy_resolved(ignore_merges, include_merges));
        assert!(resolved);
    }

    #[test]
    fn include_merges_flag_resolves_false() {
        let (ignore_merges, include_merges) =
            parse_flags(&["astro-sight", "cochange", "--include-merges"]);
        assert!(!ignore_merges);
        assert!(include_merges);
        let resolved = !include_merges;
        assert_eq!(resolved, legacy_resolved(ignore_merges, include_merges));
        assert!(
            !resolved,
            "include-merges 指定で merge を含める (ignore_merges=false)"
        );
    }

    #[test]
    fn both_flags_conflict_is_parse_error() {
        let result = Cli::try_parse_from([
            "astro-sight",
            "cochange",
            "--ignore-merges",
            "--include-merges",
        ]);
        assert!(
            result.is_err(),
            "conflicts_with により両フラグ同時指定は parse エラー"
        );
    }
}
