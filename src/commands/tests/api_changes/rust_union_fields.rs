//! Rust の公開 union のフィールドの変更 (Issue #277)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

/// `src/lib.rs` を `old` でコミットし、`new` に書き換えたときの API の差分。
fn api_changes(old: &str, new: &str) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("Cargo.toml", CARGO), ("src/lib.rs", old)], "base");
    fs::write(repo.join("src/lib.rs"), new).expect("write");
    detect_api_changes_from_worktree(repo)
}

/// `api_changes` の `name` の区分。
fn bucket(old: &str, new: &str, name: &str) -> &'static str {
    let api = api_changes(old, new);
    if api.modified.iter().any(|change| change.name == name) {
        "mod"
    } else if api
        .modified_closed_in_diff
        .iter()
        .any(|change| change.name == name)
    {
        "mod_closed"
    } else if api
        .compatible_modified
        .iter()
        .any(|change| change.name == name && change.reason == "rust_union_shape_compatible")
    {
        "mod_compat"
    } else if api.removed.iter().any(|symbol| symbol.name == name)
        || api.removed_dead.iter().any(|symbol| symbol.name == name)
    {
        "rm"
    } else {
        "none"
    }
}

/// 期待する区分の表を確かめる。
fn assert_buckets(cases: &[(String, String, &str)]) {
    for (old, new, expected) in cases {
        assert_eq!(bucket(old, new, "Value"), *expected, "{old}\n=>\n{new}");
    }
}

/// `pub union Value { .. }` (前に `attributes`、型の引数 `generics`)。
fn value(attributes: &str, generics: &str, body: &str) -> String {
    format!("{attributes}pub union Value{generics} {{\n{body}\n}}\n")
}

const VALUE: &str = "pub union Value {\n    pub number: u32,\n    pub float: f32,\n}\n\npub fn make() -> Value {\n    Value { number: 1 }\n}\n";

/// 公開フィールドの削除・改名・型の変更・公開でなくすことは、同じ差分で中の利用を直しても api.mod
/// (Issue #277 の再現)。
#[test]
fn rust_union_field_changes_are_modified() {
    let make = |body: &str, init: &str| {
        format!(
            "pub union Value {{\n{body}\n}}\n\npub fn make() -> Value {{\n    Value {{ {init} }}\n}}\n"
        )
    };
    assert_buckets(&[
        // フィールドの削除
        (
            VALUE.to_owned(),
            make("    pub number: u32,", "number: 1"),
            "mod",
        ),
        (
            VALUE.to_owned(),
            make("    pub float: f32,", "float: 1.0"),
            "mod",
        ),
        // フィールドの改名 (中の利用も直す)
        (
            VALUE.to_owned(),
            make("    pub count: u32,\n    pub float: f32,", "count: 1"),
            "mod",
        ),
        // フィールドの型の変更
        (
            VALUE.to_owned(),
            make("    pub number: u64,\n    pub float: f32,", "number: 1"),
            "mod",
        ),
        // 公開でなくすこと
        (
            VALUE.to_owned(),
            make("    number: u32,\n    pub float: f32,", "number: 1"),
            "mod",
        ),
        (
            VALUE.to_owned(),
            make(
                "    pub(crate) number: u32,\n    pub float: f32,",
                "number: 1",
            ),
            "mod",
        ),
    ]);
}

/// 外の利用者を壊し得る変更は blocking。
#[test]
fn rust_union_breaking_changes_are_modified() {
    let plain = "    pub number: u32,";
    assert_buckets(&[
        // `#[repr]` を外すことと変えること (`packed`・`align` の付け外しを含む)
        (
            value("#[repr(C)]\n", "", plain),
            value("", "", plain),
            "mod",
        ),
        (
            value("#[repr(C)]\n", "", plain),
            value("#[repr(C, packed)]\n", "", plain),
            "mod",
        ),
        (
            value("#[repr(C)]\n", "", plain),
            value("#[repr(C, align(8))]\n", "", plain),
            "mod",
        ),
        (
            value("", "", plain),
            value("#[repr(packed)]\n", "", plain),
            "mod",
        ),
        // `#[repr]` の付いた公開フィールドだけの union へのフィールドの追加 (大きさ・アラインメント・
        // 値渡しの ABI が変わり得る)。公開でないフィールドでも同じ
        (
            value("#[repr(C)]\n", "", plain),
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    pub wide: u64,",
            ),
            "mod",
        ),
        (
            value("#[repr(C)]\n", "", plain),
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    raw: [u8; 8],",
            ),
            "mod",
        ),
        // 外から作れる union への型の引数の追加。値は 1 つのフィールドで作り、式の推論に既定値は
        // 使われないので、`Value { number: 1 }` が E0282 になり得る (フィールドの型が引数を含んでも、
        // `ManuallyDrop::new(None)` のような初期化からは推論できない)
        (
            value("", "", plain),
            value(
                "",
                "<T = u32>",
                "    pub number: u32,\n    pub extra: std::mem::ManuallyDrop<T>,",
            ),
            "mod",
        ),
        (
            value(
                "",
                "",
                "    pub number: std::mem::ManuallyDrop<u32>,\n    pub float: f32,",
            ),
            value(
                "",
                "<T = u32>",
                "    pub number: std::mem::ManuallyDrop<T>,\n    pub float: f32,",
            ),
            "mod",
        ),
        (
            value(
                "",
                "",
                "    pub number: std::mem::ManuallyDrop<Option<u32>>,",
            ),
            value(
                "",
                "<T = u32>",
                "    pub number: std::mem::ManuallyDrop<Option<T>>,",
            ),
            "mod",
        ),
        // 既定値の無い型の引数の追加と、トレイトの境界を強めること
        (
            value("", "", plain),
            value("", "<T: Copy>", "    pub number: u32,\n    pub extra: T,"),
            "mod",
        ),
        (
            value("", "<T: Copy>", "    pub number: T,"),
            value("", "<T: Copy + Default>", "    pub number: T,"),
            "mod",
        ),
    ]);
}

/// 外の利用者を壊さずに広げる変更は mod_compat。
#[test]
fn rust_union_compatible_changes_are_compatible() {
    let plain = "    pub number: u32,";
    assert_buckets(&[
        // 公開フィールドの追加 (値は 1 つのフィールドで作り、パターンも 1 つのフィールドを書く)
        (
            value("", "", plain),
            value("", "", "    pub number: u32,\n    pub float: f32,"),
            "mod_compat",
        ),
        // 公開でないフィールドが有る `repr(C)` の union は、公開の API がレイアウトを決めない
        (
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    raw: [u8; 4],",
            ),
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    raw: [u8; 4],\n    pub float: f32,",
            ),
            "mod_compat",
        ),
        // `#[repr]` の付いた公開フィールドだけの union でも、旧いフィールドと同じ型のフィールドは
        // 大きさ・アラインメント・値渡しの ABI を変えない
        (
            value("#[repr(C)]\n", "", plain),
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    pub alias: u32,",
            ),
            "mod_compat",
        ),
        // 公開でないフィールドを公開すること
        (
            value("", "", "    pub number: u32,\n    float: f32,"),
            value("", "", "    pub number: u32,\n    pub float: f32,"),
            "mod_compat",
        ),
        (
            value("", "", "    pub number: u32,\n    pub(crate) float: f32,"),
            value("", "", "    pub number: u32,\n    pub float: f32,"),
            "mod_compat",
        ),
        // 既定の表現に `repr(C)` を付けること (`cfg_attr` でも)
        (
            value("", "", plain),
            value("#[repr(C)]\n", "", plain),
            "mod_compat",
        ),
        (
            value("", "", plain),
            value("#[cfg_attr(feature = \"ffi\", repr(C))]\n", "", plain),
            "mod_compat",
        ),
        // cfg を外して、無かった設定にもフィールドを足すこと
        (
            value(
                "",
                "",
                "    pub number: u32,\n    #[cfg(feature = \"float\")]\n    pub float: f32,",
            ),
            value("", "", "    pub number: u32,\n    pub float: f32,"),
            "mod_compat",
        ),
        // トレイトの境界を緩めることと、外から作れない union への既定値の付いた型の引数の追加
        (
            value("", "<T: Copy + Default>", "    pub number: T,"),
            value("", "<T: Copy>", "    pub number: T,"),
            "mod_compat",
        ),
        (
            value("", "", "    number: u32,"),
            value(
                "",
                "<T = u32>",
                "    number: u32,\n    extra: std::mem::ManuallyDrop<T>,",
            ),
            "mod_compat",
        ),
    ]);
}

/// 公開の形の変わらない変更は報告しない。
#[test]
fn rust_union_unchanged_shapes_are_not_reported() {
    let two = "    pub number: u32,\n    pub float: f32,";
    let swapped = "    pub float: f32,\n    pub number: u32,";
    assert_buckets(&[
        // フィールドの並べ替え (`repr(C)` では全てのフィールドが先頭に置かれ、既定の表現では
        // レイアウトが決まっていない)
        (value("", "", two), value("", "", swapped), "none"),
        (
            value("#[repr(C)]\n", "", two),
            value("#[repr(C)]\n", "", swapped),
            "none",
        ),
        // 既定の表現の union の公開でないフィールドの追加・削除・型の変更
        (
            value("", "", "    pub number: u32,"),
            value("", "", "    pub number: u32,\n    raw: [u8; 4],"),
            "none",
        ),
        (
            value("", "", "    pub number: u32,\n    raw: [u8; 4],"),
            value("", "", "    pub number: u32,"),
            "none",
        ),
        // 公開でないフィールドが既に有る `repr(C)` の union の公開でない部分の変更
        (
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    raw: [u8; 4],",
            ),
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    raw: [u16; 2],\n    spare: u8,",
            ),
            "none",
        ),
        // ドキュメント・derive・`repr(Rust)`・`#[repr]` の書き方
        (
            value("", "", two),
            value(
                "/// Values.\n#[derive(Clone, Copy)]\n#[repr(Rust)]\n",
                "",
                "    /// The number.\n    pub number: u32,\n    pub float: f32,",
            ),
            "none",
        ),
        (
            value("#[repr(C, align(8))]\n", "", two),
            value("#[repr(align(0x8))]\n#[repr(C)]\n", "", two),
            "none",
        ),
        (
            value("#[repr(packed)]\n", "", two),
            value("#[repr(packed(1))]\n", "", two),
            "none",
        ),
        // 型の引数の改名と整形
        (
            value("", "<T>", "    pub value: std::mem::ManuallyDrop<T>,"),
            "pub union Value<U> { pub value: std::mem::ManuallyDrop<U> }\n".to_owned(),
            "none",
        ),
        // どの設定でも同じになる cfg の書き換え
        (
            value(
                "",
                "",
                "    pub number: u32,\n    #[cfg(feature = \"float\")]\n    pub float: f32,",
            ),
            value(
                "",
                "",
                "    pub number: u32,\n    #[cfg(all(feature = \"float\"))]\n    pub float: f32,",
            ),
            "none",
        ),
    ]);
}

/// cfg の付いたフィールドと `#[repr]` は、設定ごとに対応させて比べる。
#[test]
fn rust_union_fields_are_compared_per_configuration() {
    let split = |narrow: &str| {
        value(
            "",
            "",
            &format!(
                "    #[cfg(feature = \"wide\")]\n    pub value: u64,\n    #[cfg(not(feature = \"wide\"))]\n    pub value: {narrow},"
            ),
        )
    };
    assert_buckets(&[
        // cfg で分けた同じ名前のフィールドは、それぞれ比べる
        (split("u32"), split("u16"), "mod"),
        // ある設定で公開フィールドを無くす
        (
            value("", "", "    pub number: u32,\n    pub float: f32,"),
            value(
                "",
                "",
                "    pub number: u32,\n    #[cfg(feature = \"float\")]\n    pub float: f32,",
            ),
            "mod",
        ),
        (
            value("", "", "    pub number: u32,\n    pub float: f32,"),
            value(
                "",
                "",
                "    pub number: u32,\n    #[cfg_attr(feature = \"slim\", cfg(feature = \"float\"))]\n    pub float: f32,",
            ),
            "mod",
        ),
        // 公開でないフィールドが無くなる設定では、`repr(C)` の union の公開の型がレイアウトを決める
        (
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    #[cfg(feature = \"raw\")]\n    raw: [u8; 8],",
            ),
            value(
                "#[repr(C)]\n",
                "",
                "    pub number: u32,\n    #[cfg(feature = \"raw\")]\n    raw: [u8; 8],\n    pub wide: u64,",
            ),
            "mod",
        ),
        // 旧くビルドできた設定で、新しい側に同じ名前のフィールドが 2 つ有る
        (
            value("", "", "    pub number: u32,\n    pub float: f32,"),
            value(
                "",
                "",
                "    pub number: u32,\n    pub float: f32,\n    #[cfg(feature = \"x\")]\n    pub number: u64,",
            ),
            "mod",
        ),
        // 旧い側がビルドできない設定 (同じ名前が 2 つ) は比べない
        (
            value(
                "",
                "",
                "    pub number: u32,\n    #[cfg(feature = \"x\")]\n    pub number: u64,",
            ),
            value("", "", "    pub number: u32,"),
            "none",
        ),
        // 1 つの値しか取らない鍵: union 自身が linux だけに有るので、windows だけのフィールドは
        // 比べる設定に無い
        (
            value(
                "#[cfg(target_os = \"linux\")]\n",
                "",
                "    pub number: u32,\n    #[cfg(target_os = \"windows\")]\n    pub float: f32,",
            ),
            value(
                "#[cfg(target_os = \"linux\")]\n",
                "",
                "    pub number: u32,",
            ),
            "none",
        ),
        // 条件付きの表現を外すのは壊す
        (
            value(
                "#[cfg_attr(feature = \"ffi\", repr(C))]\n",
                "",
                "    pub number: u32,",
            ),
            value("", "", "    pub number: u32,"),
            "mod",
        ),
    ]);
}

/// union 自身の cfg が絞る設定の中で比べ、述語の原子が 16 を超えれば blocking にする。
#[test]
fn rust_union_configurations_follow_rust_cfg_semantics() {
    let fields = |count: usize, extra: &str| -> String {
        let mut body: String = (0..count)
            .map(|index| format!("    #[cfg(feature = \"f{index}\")]\n    pub f{index}: u8,\n"))
            .collect();
        body.push_str("    pub number: u32,");
        body.push_str(extra);
        body
    };
    assert_buckets(&[
        // union が有る設定 (ffi) では、条件付きの表現はいつも効くので、無条件に書き換えても同じ
        (
            value(
                "#[cfg(feature = \"ffi\")]\n#[cfg_attr(feature = \"ffi\", repr(C))]\n",
                "",
                "    pub number: u32,",
            ),
            value(
                "#[cfg(feature = \"ffi\")]\n#[repr(C)]\n",
                "",
                "    pub number: u32,",
            ),
            "none",
        ),
        // 推論される寿命の境界も、設定ごとに有るフィールドから求める
        (
            value(
                "",
                "<'a, T>",
                "    #[cfg(feature = \"x\")]\n    pub value: &'a T,\n    #[cfg(not(feature = \"x\"))]\n    pub value: &'a T,",
            ),
            value("", "<'a, T>", "    pub value: &'a T,"),
            "none",
        ),
        // 原子が 16 までなら設定ごとに比べ、超えれば blocking
        (
            value("", "", &fields(16, "")),
            value("", "", &fields(16, "\n    pub extra: u8,")),
            "mod_compat",
        ),
        (
            value("", "", &fields(17, "")),
            value("", "", &fields(17, "\n    pub extra: u8,")),
            "mod",
        ),
    ]);
}

/// 公開の union を `pub(crate)` にすると、外からは削除と同じ。
#[test]
fn rust_union_restricted_visibility_is_removal() {
    let new = "pub(crate) union Value {\n    pub number: u32,\n    pub float: f32,\n}\n\npub(crate) fn make() -> Value {\n    Value { number: 1 }\n}\n";
    assert_eq!(bucket(VALUE, new, "Value"), "rm");
}

/// フィールドを並べ替えて別のファイルへ移しても、削除・追加・変更として報告しない (公開の経路は
/// `pub use` で保つ)。
#[test]
fn rust_union_reordered_while_moving_is_not_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", "mod a;\npub use a::Value;\n"),
            (
                "src/a.rs",
                "pub union Value {\n    pub number: u32,\n    pub float: f32,\n}\n",
            ),
        ],
        "base",
    );
    fs::remove_file(repo.join("src/a.rs")).expect("remove");
    fs::write(repo.join("src/lib.rs"), "mod b;\npub use b::Value;\n").expect("write");
    fs::write(
        repo.join("src/b.rs"),
        "pub union Value {\n    pub float: f32,\n    pub number: u32,\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        !api.removed.iter().any(|symbol| symbol.name == "Value")
            && !api.removed_dead.iter().any(|symbol| symbol.name == "Value")
            && !api.added.iter().any(|symbol| symbol.name == "Value")
            && !api.modified.iter().any(|change| change.name == "Value"),
        "{api:?}"
    );
}

/// 公開 union の追加と削除も、構造体と同じく api_changes に出る。`pub(crate)` の union の inherent
/// メソッドは外から呼べないので、外部の API にしない。
#[test]
fn rust_union_items_and_owned_methods_follow_visibility() {
    let api = api_changes(
        "pub fn make() -> u32 {\n    1\n}\n",
        "pub union Value {\n    pub number: u32,\n}\n\npub fn make() -> u32 {\n    1\n}\n",
    );
    assert!(
        api.added.iter().any(|symbol| symbol.name == "Value"),
        "{api:?}"
    );
    assert_eq!(
        bucket(VALUE, "pub fn make() -> u32 {\n    1\n}\n", "Value"),
        "rm"
    );
    // 同じ名前のメソッドを別のファイルに置き、同じファイルの中だけの利用による除外を外す
    // (構造体の `detect_api_changes_owner_visibility_uses_ast_not_line_text` と同じ形)。
    for (visibility, expect_flagged) in [("pub", true), ("pub(crate)", false)] {
        let source = |parameters: &str, call: &str| {
            format!(
                "{visibility} union Holder {{\n    pub value: u8,\n}}\n\nimpl Holder {{\n    pub fn value(&self, {parameters}) -> u8 {{\n        key.len() as u8\n    }}\n}}\n\npub fn entry() -> u8 {{\n    Holder {{ value: 1 }}.value({call})\n}}\n"
            )
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let before = source("key: &str", "\"a\"");
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", CARGO),
                ("src/lib.rs", "pub mod holder;\npub mod twin;\n"),
                (
                    "src/twin.rs",
                    "pub struct Twin;\n\nimpl Twin {\n    pub fn value(&self) -> u8 { 0 }\n}\n",
                ),
                ("src/holder.rs", before.as_str()),
            ],
            "base",
        );
        fs::write(
            repo.join("src/holder.rs"),
            source("key: &str, extra: u8", "\"a\", 1"),
        )
        .expect("write");
        let api = detect_api_changes_from_worktree(repo);
        let reported = api
            .modified
            .iter()
            .any(|change| change.name.ends_with("value"));
        assert_eq!(reported, expect_flagged, "{visibility}: {api:?}");
    }
}
