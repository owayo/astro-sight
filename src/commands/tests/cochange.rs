//! 共変更検出 (`detect_missing_cochanges`) と CLI フラグ等価性のテスト。

#[allow(unused_imports)]
use crate::commands::review::missing_cochange::*;
#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::engine::cochange::CoChangeExclude;
#[allow(unused_imports)]
use crate::models::dependency_files::DEPENDENCY_ECOSYSTEMS;
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
// 正本テーブルと cochange 候補除外の整合
// ------------------------------------------------------------------

/// 正本テーブル (`DEPENDENCY_MANIFEST_LOCK_PAIRS`) の全ロックファイルが cochange の
/// 候補除外に一致することを固定する。
///
/// 除外を glob 文字列で手並べしていた時代は `uv.lock` / `poetry.lock` / `pdm.lock` /
/// `Gemfile.lock` / `go.sum` / `mix.lock` の 6 個が抜けており、「Rust / npm では lock の
/// 誤検出が出ないが Python / Ruby / Go / Elixir では出る」という言語依存の非一貫性に
/// なっていた。ペアを 1 行足したら除外も追随することをテストで保証する。
#[test]
fn cochange_exclude_matches_every_dependency_lock_in_canonical_table() {
    let exclude = CoChangeExclude::build(&[]).expect("build exclude matcher");
    for eco in DEPENDENCY_ECOSYSTEMS {
        assert!(
            !eco.locks.is_empty(),
            "{} に lock が 1 つも無いのは表の記述漏れ",
            eco.manifest
        );
        for lock in eco.locks {
            assert!(
                exclude.is_match(lock),
                "{lock} は共変更候補から除外されるべき"
            );
            // monorepo のサブディレクトリに置かれていても生成物であることは変わらない
            let nested = format!("apps/web/{lock}");
            assert!(exclude.is_match(&nested), "{nested} も除外されるべき");
        }
    }
}

/// manifest 自身は engine の候補除外に**含めない**ことを固定する。
///
/// manifest は生成物ではなく人が書く宣言なので、standalone `cochange` は
/// 「過去に一緒に変更された」という事実として出し続ける。review の推奨から外すのは
/// `detect_missing_cochanges` の policy filter の役割 (責務分離)。
#[test]
fn cochange_exclude_keeps_dependency_manifests_as_candidates() {
    let exclude = CoChangeExclude::build(&[]).expect("build exclude matcher");
    for eco in DEPENDENCY_ECOSYSTEMS {
        assert!(
            !exclude.is_match(eco.manifest),
            "{} は engine の候補には残すべき (review 側で落とす)",
            eco.manifest
        );
    }
}

/// 各エコシステムの `manifest` → `langs` の対応を期待値で固定する。
///
/// 「空でない」ことだけを見るテストでは誤った `LangId` の割り当て
/// (`pyproject.toml` に `LangId::Ruby` を書く等) を検出できない。`langs` の誤りは
/// 「除外が効かない = 誤検出が残る」または「別 ecosystem を落とす」という
/// どちらの方向にも倒れるため、対応表そのものを固定する。
/// astro-sight が解析しない Elixir の `mix.exs` だけ空スライス (lock の候補除外のみ効く)。
#[test]
fn dependency_ecosystems_map_manifests_to_expected_langs() {
    use crate::language::LangId;

    let want: Vec<(&str, Vec<LangId>)> = vec![
        ("Cargo.toml", vec![LangId::Rust]),
        (
            "package.json",
            vec![LangId::Javascript, LangId::Typescript, LangId::Tsx],
        ),
        ("pyproject.toml", vec![LangId::Python]),
        ("Gemfile", vec![LangId::Ruby]),
        ("composer.json", vec![LangId::Php]),
        ("go.mod", vec![LangId::Go]),
        // Elixir は解析対象外なので空のまま
        ("mix.exs", vec![]),
    ];

    assert_eq!(
        DEPENDENCY_ECOSYSTEMS.len(),
        want.len(),
        "エコシステムを追加したらこの照合表も更新すること"
    );
    for (manifest, langs) in &want {
        let eco = DEPENDENCY_ECOSYSTEMS
            .iter()
            .find(|e| e.manifest == *manifest)
            .unwrap_or_else(|| panic!("{manifest} のエコシステム定義が無い"));
        assert_eq!(
            eco.langs,
            langs.as_slice(),
            "{manifest} の対象言語が期待と違う"
        );
    }
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
        REVIEW_COCHANGE_MIN_SAMPLES,
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
    // support 要求も engine 既定 (2) に下げる。review の既定 3 では co=2 のこの
    // フィクスチャが落ち、検証したい「base の伝播」に到達しないため。
    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        crate::models::cochange::CoChangeOptions::default().min_samples,
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
        REVIEW_COCHANGE_MIN_SAMPLES,
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
        REVIEW_COCHANGE_MIN_SAMPLES,
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

/// review の missing_cochanges は standalone `cochange` より強い support を要求する。
///
/// 変更行 blame は分母が 2 程度になる起点が普通にあるため、「1 回だけ一緒に変わった」
/// ペアが co=2/denom=2 = confidence 1.0 として最上位に並び、レビューのたびに同じ
/// 誤検出が出ていた。engine 既定 (2) では出るペアが review 既定 (3) では落ちることを、
/// 同一フィクスチャの対照で固定する (閾値を上げただけで全部落ちる、の取り違えを防ぐ)。
#[test]
fn detect_missing_cochanges_review_policy_drops_small_support_pairs() {
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
    let repo_path = repo.to_str().expect("utf-8 path");

    // 対照: engine 既定 (min_samples=2) なら co=2 のペアが missing として出る。
    let lenient = detect_missing_cochanges(
        &service,
        repo_path,
        &changed_files,
        0.0,
        crate::models::cochange::CoChangeOptions::default().min_samples,
        Some("HEAD~2"),
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;
    assert!(
        lenient.iter().any(|m| m.file == "b.rs"),
        "engine 既定では小標本ペアが出るはず (対照が壊れるとこのテストは無意味になる)。got: {lenient:?}"
    );

    // review 既定 (3) では同じペアが落ちる。
    let strict = detect_missing_cochanges(
        &service,
        repo_path,
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        Some("HEAD~2"),
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;
    assert!(
        strict.iter().all(|m| m.file != "b.rs"),
        "review policy では co=2 のペアを出さない。got: {strict:?}"
    );
    assert!(
        strict.is_empty(),
        "このフィクスチャで残る候補は無いはず。got: {strict:?}"
    );
}

/// review 既定を満たす support (co >= 3) のペアは従来どおり検出される。
/// 上のテストと合わせて「厳しくしすぎて何も出ない」状態でないことを固定する。
#[test]
fn detect_missing_cochanges_review_policy_keeps_well_supported_pairs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let a_of = |first: u32, second: u32, third: u32| {
        format!(
            "fn a() {{\n    let first = {first};\n    let second = {second};\n    let third = {third};\n}}\n"
        )
    };
    let b_of = |first: u32, second: u32, third: u32| {
        format!(
            "fn b() {{\n    let first = {first};\n    let second = {second};\n    let third = {third};\n}}\n"
        )
    };

    git_commit_files(
        repo,
        &[("a.rs", &a_of(0, 0, 0)), ("b.rs", &b_of(0, 0, 0))],
        "initial",
    );
    // 3 回の共変更で co=3 を作る。
    for (i, (a, b)) in [
        (a_of(1, 0, 0), b_of(1, 0, 0)),
        (a_of(1, 2, 0), b_of(1, 2, 0)),
        (a_of(1, 2, 3), b_of(1, 2, 3)),
    ]
    .iter()
    .enumerate()
    {
        git_commit_files(
            repo,
            &[("a.rs", a.as_str()), ("b.rs", b.as_str())],
            &format!("pair {i}"),
        );
    }
    // a.rs だけを変更した状態を review 対象にする。
    git_commit_files(repo, &[("a.rs", &a_of(10, 2, 3))], "a only 1");
    git_commit_files(repo, &[("a.rs", &a_of(10, 20, 3))], "a only 2");
    git_commit_files(repo, &[("a.rs", &a_of(10, 20, 30))], "a only 3");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("a.rs".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        Some("HEAD~3"),
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().any(|m| m.file == "b.rs"),
        "co>=3 の十分な履歴があるペアは review でも検出されるべき。got: {missing:?}"
    );
}

// ------------------------------------------------------------------
// detect_missing_cochanges: 依存宣言ファイル ↔ ソースの条件付き相関を除外する
// ------------------------------------------------------------------

/// 依存追加を繰り返した履歴を持つリポジトリで、import を 1 行も増減させない
/// 「関数本体だけの変更」に対して manifest / lock の共変更を要求しないことを固定する。
///
/// 依存を追加するコミットでは manifest / lock / ソースが必ず一緒に変わるため履歴相関は
/// 100% になるが、その相関は「依存を追加したとき」限定の条件付きのもので、本体だけの
/// 変更には因果が無い (レポート 2026-08-18-cochange-lockfile-without-new-import)。
///
/// 対照として、同じ差分で「ソース ↔ ソース」の共変更は引き続き検出されることも
/// 同時に固定する。除外が広すぎて cochange 全体が黙る状態と区別するため
/// (対照が無いと「全部落ちている」テストが素通りする)。
#[test]
fn detect_missing_cochanges_drops_dependency_manifest_when_only_body_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 依存宣言 + ロック + ソース + ヘルパーの初期状態。
    // ヘルパーは「ソース ↔ ソース」の対照ペアを作るために毎回一緒に変更する。
    let manifest_of = |deps: &str| format!("[project]\nname = \"demo\"\ndependencies = [{deps}]\n");
    let lock_of = |n: usize| format!("# lock revision {n}\n");
    let app_of = |imports: &str, body: &str| format!("{imports}\n\ndef run():\n{body}\n");
    let helper_of = |n: usize| format!("def helper():\n    return {n}\n");

    git_commit_files(
        repo,
        &[
            ("pyproject.toml", &manifest_of("")),
            ("uv.lock", &lock_of(0)),
            ("pkg/app.py", &app_of("", "    return 1")),
            ("pkg/helper.py", &helper_of(0)),
        ],
        "initial",
    );

    // 依存追加を 3 回。毎回 manifest / lock / app / helper が一緒に変わる。
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
        git_commit_files(
            repo,
            &[
                ("pyproject.toml", &manifest_of(deps[i])),
                ("uv.lock", &lock_of(i + 1)),
                ("pkg/app.py", &app_of(imports[i], "    return 1")),
                ("pkg/helper.py", &helper_of(i + 1)),
            ],
            &format!("feat: add dependency {i}"),
        );
    }

    // 4 つ目の変更 (未コミット): app.py の関数本体だけを書き換える。
    // import 行は 1 行も増減しないので、依存宣言を触る理由が無い。
    std::fs::write(
        repo.join("pkg/app.py"),
        app_of(
            imports[2],
            "    total = 0\n    for i in range(10):\n        total += i\n    return total",
        ),
    )
    .expect("write app.py");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("pkg/app.py".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().all(|m| m.file != "pyproject.toml"),
        "import 増減の無い本体変更で pyproject.toml を要求してはならない。got: {missing:?}"
    );
    assert!(
        missing.iter().all(|m| m.file != "uv.lock"),
        "import 増減の無い本体変更で uv.lock を要求してはならない。got: {missing:?}"
    );
    // 対照: ソース ↔ ソースの共変更は落とさない
    assert!(
        missing.iter().any(|m| m.file == "pkg/helper.py"),
        "ソース同士の共変更は引き続き検出されるべき (除外が広すぎないことの対照)。got: {missing:?}"
    );
}

/// 別エコシステムの依存宣言 ↔ ソースの相関は落とさないことを固定する。
///
/// `Cargo.toml`(Rust の依存宣言) と `tools/release.py`(Python) が毎回一緒に変わっているのは
/// 依存追加による交絡ではなく、別種の暗黙の結合 (リリース手順とバージョン定義など) の可能性が
/// ある。ecosystem 一致を要求せずに落とすと、cochange 本来の目的
/// (呼び出し規約以外の暗黙の結合を拾う) を構造的に削ってしまう。
#[test]
fn detect_missing_cochanges_keeps_cross_ecosystem_manifest_pairs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let manifest_of = |v: usize| format!("[package]\nname = \"demo\"\nversion = \"0.{v}.0\"\n");
    let script_of =
        |v: usize| format!("VERSION = \"0.{v}.0\"\n\n\ndef release():\n    return VERSION\n");
    let lib_of = |n: usize| format!("pub fn run() -> i32 {{\n    {n}\n}}\n");

    git_commit_files(
        repo,
        &[
            ("Cargo.toml", &manifest_of(0)),
            ("tools/release.py", &script_of(0)),
            ("src/lib.rs", &lib_of(0)),
        ],
        "initial",
    );
    // Cargo.toml と tools/release.py が 3 回一緒に変わる (src/lib.rs も起点として動かす)
    for i in 1..=3 {
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", &manifest_of(i)),
                ("tools/release.py", &script_of(i)),
                ("src/lib.rs", &lib_of(i)),
            ],
            &format!("release 0.{i}.0"),
        );
    }

    std::fs::write(repo.join("src/lib.rs"), lib_of(99)).expect("write lib.rs");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("src/lib.rs".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    // Cargo.toml (Rust の依存宣言) ↔ src/lib.rs (Rust) は同一 ecosystem なので落ちる
    assert!(
        missing.iter().all(|m| m.file != "Cargo.toml"),
        "同一 ecosystem の manifest は落とす。got: {missing:?}"
    );
    // tools/release.py は Rust の依存宣言と ecosystem が違うので残る
    assert!(
        missing.iter().any(|m| m.file == "tools/release.py"),
        "別 ecosystem のファイルとの相関は残すべき。got: {missing:?}"
    );
}

/// monorepo で別プロジェクトの manifest ↔ ソースの組を落とさないことを固定する。
///
/// `apps/web/package.json` は `apps/api/` 配下のソースの依存を宣言していないため、
/// 両者の相関は依存追加による交絡ではない (別プロジェクトが同時にリリースされる等の
/// 別の結合)。祖先ディレクトリ制約が無いと ecosystem が一致するだけで落ちてしまう。
#[test]
fn detect_missing_cochanges_keeps_manifest_outside_source_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let manifest_of =
        |v: usize| format!("{{\n  \"name\": \"web\",\n  \"version\": \"0.{v}.0\"\n}}\n");
    let api_of = |n: usize| format!("export function handler(): number {{\n  return {n};\n}}\n");

    git_commit_files(
        repo,
        &[
            ("apps/web/package.json", &manifest_of(0)),
            ("apps/api/src/main.ts", &api_of(0)),
        ],
        "initial",
    );
    for i in 1..=3 {
        git_commit_files(
            repo,
            &[
                ("apps/web/package.json", &manifest_of(i)),
                ("apps/api/src/main.ts", &api_of(i)),
            ],
            &format!("bump {i}"),
        );
    }

    std::fs::write(repo.join("apps/api/src/main.ts"), api_of(99)).expect("write main.ts");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("apps/api/src/main.ts".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().any(|m| m.file == "apps/web/package.json"),
        "別プロジェクトの manifest との相関は残すべき (祖先でないため)。got: {missing:?}"
    );
}

/// ルート manifest と入れ子 manifest が併存する monorepo で、ルート manifest との相関を
/// 落とさないことを固定する。
///
/// `apps/api/src/main.ts` の依存を宣言しているのは `apps/api/package.json` であって
/// ルートの `package.json` ではない。祖先であることだけを条件にするとルート manifest まで
/// 「依存追加による交絡」として落ち、workspace 全体のツール設定変更のような**本物の**
/// 暗黙の結合が消える。
#[test]
fn detect_missing_cochanges_keeps_root_manifest_when_nested_manifest_exists() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let root_manifest = |v: usize| {
        format!(
            "{{\n  \"name\": \"root\",\n  \"workspaces\": [\"apps/*\"],\n  \"version\": \"0.{v}.0\"\n}}\n"
        )
    };
    let api_manifest = "{\n  \"name\": \"api\"\n}\n";
    let api_of = |n: usize| format!("export function handler(): number {{\n  return {n};\n}}\n");

    git_commit_files(
        repo,
        &[
            ("package.json", &root_manifest(0)),
            ("apps/api/package.json", api_manifest),
            ("apps/api/src/main.ts", &api_of(0)),
        ],
        "initial",
    );
    // ルート manifest と api のソースが 3 回一緒に変わる (入れ子 manifest は据え置き)
    for i in 1..=3 {
        git_commit_files(
            repo,
            &[
                ("package.json", &root_manifest(i)),
                ("apps/api/src/main.ts", &api_of(i)),
            ],
            &format!("workspace tooling {i}"),
        );
    }

    std::fs::write(repo.join("apps/api/src/main.ts"), api_of(99)).expect("write main.ts");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("apps/api/src/main.ts".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().any(|m| m.file == "package.json"),
        "入れ子 manifest がある場合、ルート manifest との相関は残すべき。got: {missing:?}"
    );
}

/// 入れ子 manifest 自身との相関は (最も近い宣言元なので) 落とすことを固定する。
/// 上のテストと対にして、「近い方だけを落とす」判定が両方向で効いていることを示す。
#[test]
fn detect_missing_cochanges_drops_nearest_nested_manifest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let api_manifest =
        |deps: &str| format!("{{\n  \"name\": \"api\",\n  \"dependencies\": {{{deps}}}\n}}\n");
    let api_of = |imports: &str, body: &str| {
        format!("{imports}\nexport function handler(): number {{\n{body}\n}}\n")
    };

    git_commit_files(
        repo,
        &[
            ("package.json", "{\n  \"name\": \"root\"\n}\n"),
            ("apps/api/package.json", &api_manifest("")),
            ("apps/api/src/main.ts", &api_of("", "  return 1;")),
        ],
        "initial",
    );
    let deps = [
        "\"alpha\": \"1\"",
        "\"alpha\": \"1\", \"beta\": \"1\"",
        "\"alpha\": \"1\", \"beta\": \"1\", \"gamma\": \"1\"",
    ];
    let imports = [
        "import \"alpha\";",
        "import \"alpha\";\nimport \"beta\";",
        "import \"alpha\";\nimport \"beta\";\nimport \"gamma\";",
    ];
    for i in 0..3 {
        git_commit_files(
            repo,
            &[
                ("apps/api/package.json", &api_manifest(deps[i])),
                ("apps/api/src/main.ts", &api_of(imports[i], "  return 1;")),
            ],
            &format!("feat: dep {i}"),
        );
    }

    std::fs::write(
        repo.join("apps/api/src/main.ts"),
        api_of(
            imports[2],
            "  let t = 0;\n  for (let i = 0; i < 3; i++) t += i;\n  return t;",
        ),
    )
    .expect("write main.ts");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("apps/api/src/main.ts".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().all(|m| m.file != "apps/api/package.json"),
        "最も近い宣言元の manifest は落とすべき。got: {missing:?}"
    );
}

/// 拡張子を持たない shebang スクリプトも「ソース」として扱い、manifest の共変更要求を
/// 落とすことを固定する。
///
/// `bin/tool` のような拡張子なし Python スクリプトは astro-sight の通常解析では shebang から
/// Python として扱われる。拡張子だけで判定すると source と認識できず、同じ依存追加履歴を
/// 持つ `pyproject.toml ↔ bin/tool` の誤検出が残る。
#[test]
fn detect_missing_cochanges_treats_shebang_script_as_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let manifest_of = |deps: &str| format!("[project]\nname = \"demo\"\ndependencies = [{deps}]\n");
    let tool_of = |imports: &str, body: &str| {
        format!("#!/usr/bin/env python3\n{imports}\n\ndef main():\n{body}\n")
    };

    git_commit_files(
        repo,
        &[
            ("pyproject.toml", &manifest_of("")),
            ("bin/tool", &tool_of("", "    return 1")),
        ],
        "initial",
    );
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
        git_commit_files(
            repo,
            &[
                ("pyproject.toml", &manifest_of(deps[i])),
                ("bin/tool", &tool_of(imports[i], "    return 1")),
            ],
            &format!("feat: dep {i}"),
        );
    }

    // 本体だけ変更 (import 行の増減なし)
    std::fs::write(
        repo.join("bin/tool"),
        tool_of(
            imports[2],
            "    total = 0\n    for i in range(3):\n        total += i\n    return total",
        ),
    )
    .expect("write bin/tool");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("bin/tool".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().all(|m| m.file != "pyproject.toml"),
        "shebang スクリプトもソースとして扱い manifest を要求しない。got: {missing:?}"
    );
}

/// shebang 判定の読み込みが、256 バイト境界にマルチバイト文字が来ても壊れないことを固定する。
///
/// 256 バイト全体を `from_utf8` に通す実装では、shebang 自体は正しい ASCII なのに
/// 境界が日本語コメントの途中に落ちるだけで判定が `None` になり、`pyproject.toml ↔ bin/tool`
/// の誤検出が復活する。先頭行だけを UTF-8 化することで防ぐ。
#[test]
fn detect_missing_cochanges_shebang_probe_survives_multibyte_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let manifest_of = |deps: &str| format!("[project]\nname = \"demo\"\ndependencies = [{deps}]\n");
    // 先頭行は ASCII の shebang。2 行目以降に日本語を敷き詰めて、256 バイト境界が
    // マルチバイト文字の**途中**に落ちるようにする (「あ」= UTF-8 3 バイト)。
    //
    // 境界計算: shebang 行 "#!/usr/bin/env python3\n" = 23 バイト、続く "#" = 1 バイトで
    // プレフィックス 24 バイト。256 - 24 = 232 で 232 % 3 == 1 なので、256 バイト目は
    // 「あ」の 2 バイト目に落ちる。ここを "# " (2 バイト) にすると 231 % 3 == 0 で
    // ちょうど文字境界に一致してしまい、旧実装でもテストが通ってしまう (実際に踏んだ)。
    let tool_of = |body: &str| {
        let padding = "あ".repeat(120); // 360 バイト (256 バイト境界を確実に跨ぐ)
        format!("#!/usr/bin/env python3\n#{padding}\n\ndef main():\n{body}\n")
    };

    git_commit_files(
        repo,
        &[
            ("pyproject.toml", &manifest_of("")),
            ("bin/tool", &tool_of("    return 1")),
        ],
        "initial",
    );
    let deps = [
        "\"alpha\"",
        "\"alpha\", \"beta\"",
        "\"alpha\", \"beta\", \"gamma\"",
    ];
    for (i, dep) in deps.iter().enumerate() {
        git_commit_files(
            repo,
            &[
                ("pyproject.toml", &manifest_of(dep)),
                ("bin/tool", &tool_of(&format!("    return {}", i + 2))),
            ],
            &format!("feat: dep {i}"),
        );
    }

    std::fs::write(
        repo.join("bin/tool"),
        tool_of("    total = 0\n    for i in range(3):\n        total += i\n    return total"),
    )
    .expect("write bin/tool");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("bin/tool".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed")
    .missing;

    assert!(
        missing.iter().all(|m| m.file != "pyproject.toml"),
        "マルチバイト境界があっても shebang 判定は成立すべき。got: {missing:?}"
    );
}

/// 途中に不正バイトを含むファイルは shebang 判定の対象にしない (言語解決しない)。
///
/// `valid_up_to()` の prefix を使うと `#!/usr/bin/env python3\xff...` の `\xff` より前が
/// `from_shebang` に渡って Python と判定され、通常の言語検出が不正 UTF-8 を拒否する挙動と
/// 食い違う。加えて manifest ↔ source 警告を誤って抑制する。
///
/// `error_len() == None` (末尾で列が未完) だけを許す条件も不十分で、`error_len()` は
/// 「上限で切ったせい」ではなく「渡したスライス末尾で列が未完」しか示さないため、
/// 256 バイト未満の実 EOF 未完列まで受理してしまう。よって**先頭行の不正 UTF-8 は
/// すべて拒否する**。ここでは `error_len()` の両経路 (Some / None) を固定する。
#[test]
fn resolve_source_lang_rejects_invalid_utf8_in_first_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    // (a) 先頭行の途中に不正バイト 0xff (error_len() == Some(1) の経路)
    let mut broken = b"#!/usr/bin/env python3".to_vec();
    broken.push(0xff);
    broken.extend_from_slice(b" tail\n\nprint(1)\n");
    std::fs::write(repo.join("broken"), &broken).expect("write broken");
    // (b) 実 EOF で未完のマルチバイト列 (error_len() == None の経路)。
    //     256 バイト未満なので「上限で切ったせい」ではなく本当に壊れている。
    //     `error_len().is_none()` だけを許す条件ではこれを Python と誤判定してしまう。
    let mut truncated = b"#!/usr/bin/env python3".to_vec();
    truncated.push(0xE3); // 3 バイト列の 1 バイト目だけ
    std::fs::write(repo.join("truncated"), &truncated).expect("write truncated");
    // 対照: 同じ shebang で不正バイトが無いもの
    std::fs::write(repo.join("valid"), b"#!/usr/bin/env python3\n\nprint(1)\n")
        .expect("write valid");

    let dir_str = repo.to_str().expect("utf-8 path");
    assert!(
        resolve_source_lang_for_test(dir_str, "broken").is_none(),
        "先頭行の途中に不正バイトがあるファイルは言語解決しない"
    );
    assert!(
        resolve_source_lang_for_test(dir_str, "truncated").is_none(),
        "実 EOF で未完のマルチバイト列があるファイルも言語解決しない"
    );
    assert_eq!(
        resolve_source_lang_for_test(dir_str, "valid"),
        Some(crate::language::LangId::Python),
        "不正バイトが無ければ shebang から解決できる (対照)"
    );
}

/// symlink 経由のソースは shebang 判定の対象にしないことを固定する。
///
/// `symlink_metadata` で確認してから `File::open` する実装ではその間に実体を差し替えられる
/// (TOCTOU)。`O_NOFOLLOW` で開くため symlink は開けず、判定は「除外しない」に倒れる。
/// symlink 自体を辿らないので、リンク先が regular file でも source 扱いしない。
#[cfg(unix)]
#[test]
fn detect_missing_cochanges_does_not_follow_symlinked_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let manifest_of = |deps: &str| format!("[project]\nname = \"demo\"\ndependencies = [{deps}]\n");
    let real_of = |body: &str| format!("#!/usr/bin/env python3\n\ndef main():\n{body}\n");

    // real/tool を実体にし、bin/tool を symlink にする
    std::fs::create_dir_all(repo.join("real")).expect("mkdir real");
    std::fs::create_dir_all(repo.join("bin")).expect("mkdir bin");
    std::fs::write(repo.join("real/tool"), real_of("    return 1")).expect("write real/tool");
    std::os::unix::fs::symlink("../real/tool", repo.join("bin/tool")).expect("symlink");
    git_commit_files(repo, &[("pyproject.toml", &manifest_of(""))], "initial");

    let deps = [
        "\"alpha\"",
        "\"alpha\", \"beta\"",
        "\"alpha\", \"beta\", \"gamma\"",
    ];
    for (i, dep) in deps.iter().enumerate() {
        std::fs::write(
            repo.join("real/tool"),
            real_of(&format!("    return {}", i + 2)),
        )
        .expect("write real/tool");
        git_commit_files(
            repo,
            &[("pyproject.toml", &manifest_of(dep))],
            &format!("dep {i}"),
        );
    }

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("bin/tool".to_string());

    // symlink は source と判定されないため、除外は成立せず (= 従来どおりの挙動)。
    // ここでは panic せず結果が返ることと、symlink を辿った判定になっていないことを確認する。
    let report = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed");
    // symlink を Python と誤認していないこと = 除外が成立していないことを、
    // 内部判定関数で直接固定する (cochange の履歴条件に依存しない形で)。
    assert!(
        resolve_source_lang_for_test(repo.to_str().expect("utf-8 path"), "bin/tool").is_none(),
        "symlink は O_NOFOLLOW で開けないため言語解決しない"
    );
    assert!(
        resolve_source_lang_for_test(repo.to_str().expect("utf-8 path"), "real/tool").is_some(),
        "実体のスクリプトは shebang から言語解決できる (対照)"
    );
    let _ = report;
}

/// ロックファイルだけを変更した diff (依存更新コマンドの実行) を起点にしても、
/// 依存追加コミットで一緒に変わっていたソースを共変更相手として要求しないことを固定する。
///
/// lock は生成物なので「lock を変えたなら X も変えろ」という推奨に意味が無い。
/// engine 側の候補除外は相方側にしか効かないため、起点側は review policy で落とす。
#[test]
fn detect_missing_cochanges_ignores_lockfile_only_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let lock_of = |n: usize| format!("# lock revision {n}\n");
    let app_of = |n: usize| format!("def run():\n    return {n}\n");

    git_commit_files(
        repo,
        &[("uv.lock", &lock_of(0)), ("pkg/app.py", &app_of(0))],
        "initial",
    );
    for i in 1..=3 {
        git_commit_files(
            repo,
            &[("uv.lock", &lock_of(i)), ("pkg/app.py", &app_of(i))],
            &format!("feat: dep {i}"),
        );
    }

    // uv.lock だけを変更した状態 (uv lock --upgrade 相当)。
    std::fs::write(repo.join("uv.lock"), lock_of(99)).expect("write uv.lock");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("uv.lock".to_string());

    let report = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        REVIEW_COCHANGE_MIN_SAMPLES,
        None,
    )
    .expect("detect_missing_cochanges should succeed");

    assert!(
        report.missing.is_empty(),
        "lock のみの変更では共変更を要求しない。got: {:?}",
        report.missing
    );
}

/// `review --cochange-min-samples` の CLI 既定が review policy の定数と一致することを固定する。
/// (CLI は文字列リテラルで既定を持つため、定数だけ変えると黙ってずれる)
#[test]
fn review_cli_cochange_min_samples_default_matches_policy() {
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    let cli = Cli::try_parse_from(["astro-sight", "review", "--dir", "."])
        .expect("review args should parse");
    match cli.command {
        Commands::Review {
            cochange_min_samples,
            ..
        } => assert_eq!(cochange_min_samples, REVIEW_COCHANGE_MIN_SAMPLES),
        other => panic!("expected Review, got {other:?}"),
    }
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
