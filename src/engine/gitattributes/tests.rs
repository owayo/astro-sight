//! `.gitattributes` の解決の単体テスト。Git (`git check-attr`) を正解として比べる。

use std::process::Command;

use super::*;

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "astro-sight-tests"]);
    git(dir, &["config", "user.email", "astro-sight@example.com"]);
    // 開発機の利用者の属性ファイルを読まない (比べる git も同じ設定を読む)。
    git(
        dir,
        &["config", "core.attributesFile", ".git/no-user-attributes"],
    );
}

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// `git check-attr` の答え (`set` / `unset` / `unspecified` / 値)。
fn git_states(dir: &Path, paths: &[&str]) -> Vec<String> {
    let mut args = vec!["check-attr", "-z", "linguist-generated", "--"];
    args.extend(paths);
    let stdout = git(dir, &args);
    let fields: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    fields
        .chunks(3)
        .filter(|record| record.len() == 3)
        .map(|record| String::from_utf8_lossy(record[2]).into_owned())
        .collect()
}

fn ours(dir: &Path, paths: &[&str]) -> Vec<LinguistGenerated> {
    let resolver = AttributeResolver::open(dir);
    let mut lookup = resolver.lookup();
    paths
        .iter()
        .map(|path| lookup.linguist_generated(&dir.join(path)).unwrap())
        .collect()
}

/// `check-attr` の文字列に写す (値の `set` / `unset` は状態と区別できないので、値の
/// ケースは別のテストで状態を直接確かめる)。
fn as_check_attr(state: &LinguistGenerated) -> String {
    match state {
        LinguistGenerated::Set => "set".into(),
        LinguistGenerated::Unset => "unset".into(),
        LinguistGenerated::Unspecified => "unspecified".into(),
        LinguistGenerated::Value(value) => value.clone(),
    }
}

/// 子ディレクトリの `.gitattributes` が親より優先、同じファイルの中は後勝ち、`!` で未指定に
/// 戻す、`/` を含むパターンは置いたディレクトリ基準、`**`、マクロ、引用したパターン、
/// `$GIT_DIR/info/attributes` が最優先、を Git と同じに解決する。
#[test]
fn resolves_like_git_check_attr() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(
        root,
        ".gitattributes",
        "*.c linguist-generated\n\
         gen/** linguist-generated\n\
         /top.rs linguist-generated\n\
         keep/*.c -linguist-generated\n\
         [attr]generated linguist-generated\n\
         macro/* generated\n\
         \"quoted name.ts\" linguist-generated\n\
         value/a.ts linguist-generated=true\n\
         value/b.ts linguist-generated=false\n\
         again.ts linguist-generated\n\
         again.ts -linguist-generated\n",
    );
    write(
        root,
        "sub/.gitattributes",
        "*.c -linguist-generated\nback.c !linguist-generated\nnested/*.ts linguist-generated\n",
    );
    write(
        root,
        "sub/deeper/.gitattributes",
        "*.c linguist-generated\n",
    );
    write(
        root,
        ".git/info/attributes",
        "info.rs linguist-generated\nsub/deeper/x.c -linguist-generated\n",
    );
    let paths = [
        "a.c",
        "src/a.c",
        "gen/x.rs",
        "gen/deep/y.ts",
        "top.rs",
        "src/top.rs",
        "keep/k.c",
        "macro/m.go",
        "quoted name.ts",
        "value/a.ts",
        "value/b.ts",
        "again.ts",
        "sub/s.c",
        "sub/back.c",
        "sub/nested/n.ts",
        "nested/n.ts",
        "sub/deeper/d.c",
        "sub/deeper/x.c",
        "info.rs",
        "plain.rs",
    ];
    for path in paths {
        write(root, path, "x\n");
    }
    let expected = git_states(root, &paths);
    let actual: Vec<String> = ours(root, &paths).iter().map(as_check_attr).collect();
    assert_eq!(actual, expected);
    // 前提: 4 状態のすべてと値が現れている (空振りのテストにしない)。
    for state in ["set", "unset", "unspecified", "true", "false"] {
        assert!(expected.iter().any(|s| s == state), "{state}: {expected:?}");
    }
}

/// サブディレクトリから走査しても、作業ツリーの根とその間の `.gitattributes` を読む。
/// 照会するパスは走査したディレクトリの下のファイル。
#[test]
fn subdirectory_scan_reads_ancestor_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(
        root,
        ".gitattributes",
        "*.gen.ts linguist-generated\n/pkg/top.ts linguist-generated\n",
    );
    write(
        root,
        "pkg/.gitattributes",
        "local.ts linguist-generated\n/anchored.ts linguist-generated\n",
    );
    let paths = [
        "pkg/app/a.gen.ts",
        "pkg/local.ts",
        "pkg/app/local.ts",
        "pkg/top.ts",
        "pkg/anchored.ts",
        "pkg/app/anchored.ts",
    ];
    for path in paths {
        write(root, path, "x\n");
    }
    let expected = git_states(root, &paths);
    let sub = root.join("pkg");
    let resolver = AttributeResolver::open(&sub);
    let mut lookup = resolver.lookup();
    let actual: Vec<String> = paths
        .iter()
        .map(|path| {
            let state = lookup.linguist_generated(&root.join(path)).unwrap();
            as_check_attr(&state)
        })
        .collect();
    assert_eq!(actual, expected);
    assert!(expected.iter().any(|s| s == "unspecified"), "{expected:?}");
    // ディレクトリの外のパスは属性が無いものとする。
    assert_eq!(
        lookup
            .linguist_generated(&root.join("other.gen.ts"))
            .unwrap(),
        LinguistGenerated::Unspecified
    );
}

/// 作業ツリーに無い `.gitattributes` は index から補う (Git と同じ)。
#[test]
fn missing_worktree_attributes_fall_back_to_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, "lib/.gitattributes", "*.rs linguist-generated\n");
    write(root, "lib/a.rs", "x\n");
    git(root, &["add", "-A"]);
    std::fs::remove_file(root.join("lib/.gitattributes")).unwrap();
    assert_eq!(git_states(root, &["lib/a.rs"]), ["set"]);
    assert_eq!(ours(root, &["lib/a.rs"]), [LinguistGenerated::Set]);
}

/// Linguist の解釈: set と `false` 以外の値は生成物、unset と `false` は生成物でない、
/// 未指定は判定しない。`check-attr` では値の `set` / `unset` と状態を区別できないので、
/// 状態を直接確かめる。
#[test]
fn linguist_interpretation_of_states_and_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(
        root,
        ".gitattributes",
        "set.ts linguist-generated\n\
         unset.ts -linguist-generated\n\
         back.ts linguist-generated\n\
         back.ts !linguist-generated\n\
         true.ts linguist-generated=true\n\
         false.ts linguist-generated=false\n\
         upper.ts linguist-generated=FALSE\n\
         zero.ts linguist-generated=0\n\
         word.ts linguist-generated=unset\n\
         empty.ts linguist-generated=\n",
    );
    let cases = [
        ("set.ts", LinguistGenerated::Set, Some(true)),
        ("unset.ts", LinguistGenerated::Unset, Some(false)),
        ("back.ts", LinguistGenerated::Unspecified, None),
        ("none.ts", LinguistGenerated::Unspecified, None),
        (
            "true.ts",
            LinguistGenerated::Value("true".into()),
            Some(true),
        ),
        (
            "false.ts",
            LinguistGenerated::Value("false".into()),
            Some(false),
        ),
        (
            "upper.ts",
            LinguistGenerated::Value("FALSE".into()),
            Some(true),
        ),
        ("zero.ts", LinguistGenerated::Value("0".into()), Some(true)),
        (
            "word.ts",
            LinguistGenerated::Value("unset".into()),
            Some(true),
        ),
        (
            "empty.ts",
            LinguistGenerated::Value(String::new()),
            Some(true),
        ),
    ];
    let paths: Vec<&str> = cases.iter().map(|(path, _, _)| *path).collect();
    for ((path, state, generated), actual) in cases.iter().zip(ours(root, &paths)) {
        assert_eq!(&actual, state, "{path}");
        assert_eq!(actual.generated(), *generated, "{path}");
    }
}

/// git の管理外のディレクトリは、走査の根の下の `.gitattributes` だけを読む (入れ子も読む)。
#[test]
fn standalone_directory_reads_only_its_own_attributes() {
    let outer = tempfile::tempdir().unwrap();
    write(outer.path(), ".gitattributes", "*.ts linguist-generated\n");
    let root = outer.path().join("project");
    write(&root, ".gitattributes", "gen/** linguist-generated\n");
    write(&root, "lib/.gitattributes", "*.out.ts linguist-generated\n");
    let resolver = AttributeResolver::open(&root);
    let mut lookup = resolver.lookup();
    for (path, expected) in [
        ("gen/a.ts", LinguistGenerated::Set),
        ("lib/b.out.ts", LinguistGenerated::Set),
        ("src/c.ts", LinguistGenerated::Unspecified),
    ] {
        write(&root, path, "x\n");
        assert_eq!(
            lookup.linguist_generated(&root.join(path)).unwrap(),
            expected,
            "{path}: 管理外では親のディレクトリの属性を読まない"
        );
    }
}

/// index のスナップショットは、スナップショットの `.gitattributes` に加えて元のリポジトリの
/// info を読む。登録を外すと管理外のディレクトリと同じに戻る。
#[test]
fn registered_snapshot_reads_original_info_attributes() {
    let original = tempfile::tempdir().unwrap();
    init_repo(original.path());
    write(
        original.path(),
        ".git/info/attributes",
        "*.info.ts linguist-generated\n",
    );
    let snapshot = tempfile::tempdir().unwrap();
    write(
        snapshot.path(),
        ".gitattributes",
        "*.tree.ts linguist-generated\n",
    );
    write(snapshot.path(), "pkg/a.info.ts", "x\n");
    write(snapshot.path(), "pkg/b.tree.ts", "x\n");
    let pkg = snapshot.path().join("pkg");
    let paths = [pkg.join("a.info.ts"), pkg.join("b.tree.ts")];
    let states = |dir: &Path| {
        let resolver = AttributeResolver::open(dir);
        let mut lookup = resolver.lookup();
        paths
            .iter()
            .map(|path| lookup.linguist_generated(path).unwrap())
            .collect::<Vec<_>>()
    };
    {
        let _registration = register_snapshot(snapshot.path(), original.path());
        assert_eq!(
            states(&pkg),
            [LinguistGenerated::Set, LinguistGenerated::Set],
            "スナップショットの根の属性と元の info の両方"
        );
    }
    assert_eq!(
        states(&pkg),
        [
            LinguistGenerated::Unspecified,
            LinguistGenerated::Unspecified
        ],
        "登録を外すと pkg を根にした管理外のディレクトリになる"
    );
}

/// index を読めないときは、照会のたびに失敗を返す (除外しない側へ倒す材料)。
#[test]
fn unreadable_index_is_a_lookup_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, ".gitattributes", "*.ts linguist-generated\n");
    write(root, "a.ts", "x\n");
    git(root, &["add", "-A"]);
    std::fs::write(root.join(".git/index"), b"not an index").unwrap();
    let resolver = AttributeResolver::open(root);
    let mut lookup = resolver.lookup();
    let error = lookup.linguist_generated(&root.join("a.ts")).unwrap_err();
    assert!(error.cause.contains("index"), "{error:?}");
    assert!(error.scope.is_empty());
}

/// 読めない入れ子の `.gitattributes` は、その下のファイルの照会だけを失敗にする
/// (gix は Windows で読めない属性ファイルを「無い」と同じに扱うので自前で確かめる)。
/// 対照: 兄弟のディレクトリと根のファイルは解決できる。
#[cfg(unix)]
#[test]
fn unreadable_nested_attributes_fail_only_below_them() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write(root, ".gitattributes", "*.ts linguist-generated\n");
    write(root, "locked/.gitattributes", "*.ts -linguist-generated\n");
    write(root, "locked/a.ts", "x\n");
    write(root, "open/b.ts", "x\n");
    write(root, "c.ts", "x\n");
    let locked = root.join("locked/.gitattributes");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_ok() {
        // root で動いていて読めてしまう環境では確かめられない。
        return;
    }
    let resolver = AttributeResolver::open(root);
    let mut lookup = resolver.lookup();
    let error = lookup
        .linguist_generated(&root.join("locked/a.ts"))
        .unwrap_err();
    assert_eq!(error.scope, "locked");
    assert_eq!(
        lookup.linguist_generated(&root.join("open/b.ts")).unwrap(),
        LinguistGenerated::Set
    );
    assert_eq!(
        lookup.linguist_generated(&root.join("c.ts")).unwrap(),
        LinguistGenerated::Set
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// `**` が 10 万個続くパターンでも照合がスタックを使い切らず、すぐに終わる。
/// 子プロセスの 256 KiB のスタックで照合する。
#[test]
fn long_double_star_pattern_does_not_overflow() {
    crate::engine::tree_walk::run_deep_test_isolated(
        module_path!(),
        "long_double_star_pattern_does_not_overflow",
        || {
            let dir = tempfile::tempdir().unwrap();
            write(
                dir.path(),
                ".gitattributes",
                &format!("{}gen.ts linguist-generated\n", "**/".repeat(100_000)),
            );
            write(dir.path(), "src/deep/gen.ts", "x\n");
            write(dir.path(), "src/deep/other.ts", "x\n");
            let resolver = AttributeResolver::open(dir.path());
            let mut lookup = resolver.lookup();
            // 照合の再帰の上限を超えるので一致しない (Git と違い、落ちずに未指定になる)。
            assert!(
                lookup
                    .linguist_generated(&dir.path().join("src/deep/gen.ts"))
                    .is_ok()
            );
            assert_eq!(
                lookup
                    .linguist_generated(&dir.path().join("src/deep/other.ts"))
                    .unwrap(),
                LinguistGenerated::Unspecified
            );
        },
    );
}
