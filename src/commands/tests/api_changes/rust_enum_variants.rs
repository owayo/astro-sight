//! Rust の公開 enum の variant の変更 (Issue #276)。

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
        .any(|change| change.name == name && change.reason == "rust_enum_shape_compatible")
    {
        "mod_compat"
    } else {
        "none"
    }
}

/// 期待する区分の表を確かめる。
fn assert_buckets(cases: &[(&str, &str, &str)], name: &str) {
    for (old, new, expected) in cases {
        assert_eq!(bucket(old, new, name), *expected, "{old}\n=>\n{new}");
    }
}

const KIND: &str = "pub enum Kind {\n    Plain,\n    Tagged { tag: u32 },\n}\n\npub fn make() -> Kind {\n    Kind::Plain\n}\n";

/// variant の削除・改名、variant のフィールドの改名は、同じ差分で中の利用を直しても api.mod
/// (Issue #276 の再現)。
#[test]
fn rust_enum_variant_changes_are_modified() {
    assert_buckets(
        &[
            // variant の削除
            (
                KIND,
                "pub enum Kind {\n    Plain,\n}\n\npub fn make() -> Kind {\n    Kind::Plain\n}\n",
                "mod",
            ),
            // variant の改名 (中の利用も直す)
            (
                KIND,
                "pub enum Kind {\n    Simple,\n    Tagged { tag: u32 },\n}\n\npub fn make() -> Kind {\n    Kind::Simple\n}\n",
                "mod",
            ),
            // variant のフィールドの改名
            (
                KIND,
                "pub enum Kind {\n    Plain,\n    Tagged { label: u32 },\n}\n\npub fn make() -> Kind {\n    Kind::Plain\n}\n",
                "mod",
            ),
        ],
        "Kind",
    );
}

/// 外の利用者を壊す変更は blocking。
#[test]
fn rust_enum_breaking_changes_are_modified() {
    let tagged = |body: &str| format!("pub enum Kind {{\n{body}\n}}\n");
    let cases: Vec<(String, String)> = vec![
        // `#[non_exhaustive]` でない enum への variant の追加
        (tagged("    A,\n    B,"), tagged("    A,\n    B,\n    C,")),
        // enum への `#[non_exhaustive]` の追加
        (
            tagged("    A,\n    B,"),
            format!("#[non_exhaustive]\n{}", tagged("    A,\n    B,")),
        ),
        // variant への `#[non_exhaustive]` の追加
        (
            tagged("    A { x: u8 },"),
            tagged("    #[non_exhaustive]\n    A { x: u8 },"),
        ),
        // フィールドの型の変更
        (tagged("    A { x: u8 },"), tagged("    A { x: u16 },")),
        // `#[non_exhaustive]` でない variant へのフィールドの追加
        (tagged("    A(u8),"), tagged("    A(u8, u8),")),
        // variant の形の変更
        (tagged("    A,\n    B,"), tagged("    A(),\n    B,")),
        // 外から `as` で読める値の変更
        (tagged("    A = 1,\n    B,"), tagged("    A = 2,\n    B,")),
        // `#[non_exhaustive]` でも、途中への追加は後ろの暗黙の値をずらす
        (
            format!("#[non_exhaustive]\n{}", tagged("    A,\n    B,")),
            format!("#[non_exhaustive]\n{}", tagged("    A,\n    X,\n    B,")),
        ),
        // `#[non_exhaustive]` でも、データを持つ variant の追加は既存の `as` を壊す
        (
            format!("#[non_exhaustive]\n{}", tagged("    A,\n    B,")),
            format!(
                "#[non_exhaustive]\n{}",
                tagged("    A,\n    B,\n    C(u8),")
            ),
        ),
        // `#[repr]` を外すことと変えること
        (
            format!("#[repr(u8)]\n{}", tagged("    A,\n    B,")),
            tagged("    A,\n    B,"),
        ),
        (
            format!("#[repr(u8)]\n{}", tagged("    A,\n    B,")),
            format!("#[repr(u16)]\n{}", tagged("    A,\n    B,")),
        ),
        // `repr(u8)` のデータを持つ enum の、名前付きのフィールドの並べ替え
        (
            format!("#[repr(u8)]\n{}", tagged("    A { x: u8, y: u16 },")),
            format!("#[repr(u8)]\n{}", tagged("    A { y: u16, x: u8 },")),
        ),
    ];
    for (old, new) in &cases {
        assert_eq!(bucket(old, new, "Kind"), "mod", "{old}\n=>\n{new}");
    }
}

/// 外の利用者を壊さずに広げるか、制限を緩めるだけの変更は mod_compat。
#[test]
fn rust_enum_compatible_changes_are_compatible() {
    let tagged = |body: &str| format!("pub enum Kind {{\n{body}\n}}\n");
    let exhaustive = |body: &str| format!("#[non_exhaustive]\n{}", tagged(body));
    let cases: Vec<(String, String)> = vec![
        // `#[non_exhaustive]` の enum の末尾への variant の追加
        (
            exhaustive("    A,\n    B,"),
            exhaustive("    A,\n    B,\n    C,"),
        ),
        // enum の `#[non_exhaustive]` を外す
        (exhaustive("    A,\n    B,"), tagged("    A,\n    B,")),
        // `#[non_exhaustive]` の variant への名前付きのフィールドの追加
        (
            tagged("    #[non_exhaustive]\n    A { x: u8 },"),
            tagged("    #[non_exhaustive]\n    A { x: u8, y: u8 },"),
        ),
        // `#[non_exhaustive]` の variant のタプルの末尾への追加
        (
            tagged("    #[non_exhaustive]\n    A(u8),"),
            tagged("    #[non_exhaustive]\n    A(u8, u16),"),
        ),
        // 既定の表現に `repr(u8)` を付ける
        (
            tagged("    A,\n    B,"),
            format!("#[repr(u8)]\n{}", tagged("    A,\n    B,")),
        ),
    ];
    for (old, new) in &cases {
        assert_eq!(bucket(old, new, "Kind"), "mod_compat", "{old}\n=>\n{new}");
    }
}

/// 公開の形の変わらない変更は報告しない。
#[test]
fn rust_enum_unchanged_shapes_are_not_reported() {
    let tagged = |body: &str| format!("pub enum Kind {{\n{body}\n}}\n");
    let cases: Vec<(String, String)> = vec![
        // 値が外から観測できない enum の variant の並べ替え
        (
            tagged("    A(u8),\n    B { x: u8 },"),
            tagged("    B { x: u8 },\n    A(u8),"),
        ),
        // ドキュメントコメント・derive・helper 属性・`#[doc(hidden)]`
        (
            tagged("    A,\n    B,"),
            format!(
                "/// Kinds.\n#[derive(Debug, Default)]\n{}",
                tagged("    /// The first.\n    #[default]\n    A,\n    #[doc(hidden)]\n    B,")
            ),
        ),
        // `repr` の無い enum の名前付きのフィールドの並べ替え
        (
            tagged("    A { x: u8, y: u16 },"),
            tagged("    A { y: u16, x: u8 },"),
        ),
        // 値の同じ discriminant の書き方
        (tagged("    A,\n    B,"), tagged("    A = 0,\n    B = 0x1,")),
        // 整形
        (
            tagged("    A { x: u8 },\n    B,"),
            "pub enum Kind { A { x: u8 }, B }\n".to_owned(),
        ),
    ];
    for (old, new) in &cases {
        assert_eq!(bucket(old, new, "Kind"), "none", "{old}\n=>\n{new}");
    }
}

/// cfg の付いた variant とフィールドは、設定ごとに対応させて比べる。
#[test]
fn rust_enum_conditional_variants_follow_each_configuration() {
    let cases: Vec<(&str, &str, &str)> = vec![
        // cfg で外れるデータを持つ variant があっても、外れた設定では `as` で読めるので、並べ替えは壊す
        (
            "pub enum Kind {\n    A,\n    B,\n    #[cfg(feature = \"payload\")]\n    Data(u8),\n}\n",
            "pub enum Kind {\n    B,\n    A,\n    #[cfg(feature = \"payload\")]\n    Data(u8),\n}\n",
            "mod",
        ),
        // cfg で分けた同じ名前のフィールドは、それぞれ比べる
        (
            "pub enum Kind {\n    V {\n        #[cfg(feature = \"wide\")]\n        value: u32,\n        #[cfg(not(feature = \"wide\"))]\n        value: u8,\n    },\n}\n",
            "pub enum Kind {\n    V {\n        #[cfg(feature = \"wide\")]\n        value: u64,\n        #[cfg(not(feature = \"wide\"))]\n        value: u8,\n    },\n}\n",
            "mod",
        ),
        // cfg で分けた同じ名前の variant も対応させるので、型の引数の改名は変更ではない
        (
            "pub enum Kind<T> {\n    #[cfg(feature = \"wide\")]\n    Value(T),\n    #[cfg(not(feature = \"wide\"))]\n    Value(T),\n}\n",
            "pub enum Kind<U> {\n    #[cfg(feature = \"wide\")]\n    Value(U),\n    #[cfg(not(feature = \"wide\"))]\n    Value(U),\n}\n",
            "none",
        ),
        // cfg の付いた variant のある `#[non_exhaustive]` の enum でも、末尾への追加は旧い値を変えない
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"extra\")]\n    B,\n    C,\n}\n",
            "mod_compat",
        ),
        // 同じ値の書き換え
        (
            "pub enum Kind {\n    A = 1,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "pub enum Kind {\n    A = 0x1,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "none",
        ),
        // 途中への追加は、cfg のある enum でも壊す
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    C,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "mod",
        ),
    ];
    for (old, new, expected) in &cases {
        assert_eq!(bucket(old, new, "Kind"), *expected, "{old}\n=>\n{new}");
    }
}

/// フィールドの無い `#[non_exhaustive]` の variant にフィールドを持たせる変更と、`cfg_attr` で
/// 定義した表現を付ける変更は、今の利用を壊さない。`repr(u128)` の大きな値も比べる。
#[test]
fn rust_enum_restricted_variants_conditional_reprs_and_wide_values() {
    let cases: Vec<(&str, &str, &str)> = vec![
        (
            "pub enum Kind {\n    A,\n    #[non_exhaustive]\n    Other,\n}\n",
            "pub enum Kind {\n    A,\n    #[non_exhaustive]\n    Other { code: u8 },\n}\n",
            "mod_compat",
        ),
        (
            "pub enum Kind {\n    Value(u8),\n}\n",
            "#[cfg_attr(feature = \"ffi\", repr(C))]\npub enum Kind {\n    Value(u8),\n}\n",
            "mod_compat",
        ),
        // 条件付きの表現を外すのは壊す
        (
            "#[cfg_attr(feature = \"ffi\", repr(C))]\npub enum Kind {\n    Value(u8),\n}\n",
            "pub enum Kind {\n    Value(u8),\n}\n",
            "mod",
        ),
        // `i128` に収まらない値を書いても、値が同じなら変更ではない
        (
            "#[repr(u128)]\npub enum Kind {\n    A = 170141183460469231731687303715884105727,\n    B,\n}\n",
            "#[repr(u128)]\npub enum Kind {\n    A = 170141183460469231731687303715884105727,\n    B = 170141183460469231731687303715884105728,\n}\n",
            "none",
        ),
    ];
    for (old, new, expected) in &cases {
        assert_eq!(bucket(old, new, "Kind"), *expected, "{old}\n=>\n{new}");
    }
}

/// 値を観測できない enum は、variant を並べ替えて別のファイルへ移しても、削除・追加・変更として
/// 報告しない (公開の経路は `pub use` で保つ)。
#[test]
fn rust_enum_reordered_while_moving_is_not_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", "mod a;\npub use a::Kind;\n"),
            ("src/a.rs", "pub enum Kind {\n    A(u8),\n    B(u8),\n}\n"),
        ],
        "base",
    );
    fs::remove_file(repo.join("src/a.rs")).expect("remove");
    fs::write(repo.join("src/lib.rs"), "mod b;\npub use b::Kind;\n").expect("write");
    fs::write(
        repo.join("src/b.rs"),
        "pub enum Kind {\n    B(u8),\n    A(u8),\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        !api.removed.iter().any(|symbol| symbol.name == "Kind")
            && !api.removed_dead.iter().any(|symbol| symbol.name == "Kind")
            && !api.added.iter().any(|symbol| symbol.name == "Kind")
            && !api.modified.iter().any(|change| change.name == "Kind"),
        "{api:?}"
    );
}

/// `as` を妨げるものと discriminant の値は、cfg の設定ごとに比べる。cfg を外して常に有る variant に
/// するのと、条件付きの表現を広げるのは、今の利用を壊さない。
#[test]
fn rust_enum_casts_values_and_reprs_follow_each_configuration() {
    let cases: Vec<(&str, &str, &str)> = vec![
        // 別の feature で有効になるデータを持つ variant を足すと、その設定で `as` を失う
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"payload\")]\n    Data(u8),\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"payload\")]\n    Data(u8),\n    #[cfg(feature = \"extra\")]\n    Extra(u8),\n}\n",
            "mod",
        ),
        // cfg と cfg(not) で分けたフィールドは、どの設定でも有るので値は観測できない
        (
            "pub enum Kind {\n    A {\n        #[cfg(feature = \"wide\")]\n        value: u32,\n        #[cfg(not(feature = \"wide\"))]\n        value: u8,\n    },\n    B,\n}\n",
            "pub enum Kind {\n    B,\n    A {\n        #[cfg(feature = \"wide\")]\n        value: u32,\n        #[cfg(not(feature = \"wide\"))]\n        value: u8,\n    },\n}\n",
            "none",
        ),
        // 暗黙の値を同じ値で書く
        (
            "pub enum Kind {\n    A,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "pub enum Kind {\n    A = 0,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "none",
        ),
        // 書いた値で既存の値を保つ途中への追加
        (
            "#[non_exhaustive]\npub enum Kind {\n    A = 1,\n    #[cfg(feature = \"extra\")]\n    B = 10,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A = 1,\n    X = 5,\n    #[cfg(feature = \"extra\")]\n    B = 10,\n}\n",
            "mod_compat",
        ),
        // cfg を外して、どの設定でも有る variant にする
        (
            "#[non_exhaustive]\npub enum Kind {\n    Data(u8),\n    #[cfg(feature = \"extra\")]\n    Ready,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    Data(u8),\n    Ready,\n}\n",
            "mod_compat",
        ),
        // `#[non_exhaustive]` でない enum では、無かった設定の網羅的な照合を壊す
        (
            "pub enum Kind {\n    Data(u8),\n    #[cfg(feature = \"extra\")]\n    Ready,\n}\n",
            "pub enum Kind {\n    Data(u8),\n    Ready,\n}\n",
            "mod",
        ),
        // 条件付きの `repr(C)` を無条件にする
        (
            "#[cfg_attr(feature = \"ffi\", repr(C))]\npub enum Kind {\n    A(u8),\n    B(u16),\n}\n",
            "#[repr(C)]\npub enum Kind {\n    A(u8),\n    B(u16),\n}\n",
            "mod_compat",
        ),
        // 既定の表現に、無条件と条件付きの定義した表現を付ける
        (
            "pub enum Kind {\n    A(u8),\n    B(u16),\n}\n",
            "#[repr(C)]\n#[cfg_attr(compact, repr(u8))]\npub enum Kind {\n    A(u8),\n    B(u16),\n}\n",
            "mod_compat",
        ),
        // `repr(u8)` で、cfg で分けた同じ名前のフィールドの並びを入れ替える
        (
            "#[repr(u8)]\npub enum Kind {\n    V {\n        #[cfg(wide)]\n        value: u32,\n        flag: u8,\n        #[cfg(not(wide))]\n        value: u16,\n    },\n}\n",
            "#[repr(u8)]\npub enum Kind {\n    V {\n        #[cfg(not(wide))]\n        value: u16,\n        flag: u8,\n        #[cfg(wide)]\n        value: u32,\n    },\n}\n",
            "mod",
        ),
    ];
    for (old, new, expected) in &cases {
        assert_eq!(bucket(old, new, "Kind"), *expected, "{old}\n=>\n{new}");
    }
}

/// cfg の述語を読み、設定ごとに比べる (どの設定でも今の利用を壊さない変更は blocking にしない)。
#[test]
fn rust_enum_changes_are_compared_per_configuration() {
    let cases: Vec<(&str, &str, &str)> = vec![
        // 末尾の variant から cfg を外す (有った設定では値も同じ)
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    B,\n}\n",
            "mod_compat",
        ),
        // cfg の付いた variant の暗黙の値を同じ値で書く
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"extra\")]\n    B,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"extra\")]\n    B = 1,\n}\n",
            "none",
        ),
        // 書いた値で後ろの値を保つ途中への追加 (cfg の付いた値の後ろも)
        (
            "#[non_exhaustive]\npub enum Kind {\n    A = 0,\n    B = 10,\n    #[cfg(feature = \"extra\")]\n    C = 20,\n    D,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A = 0,\n    X = 5,\n    B = 10,\n    #[cfg(feature = \"extra\")]\n    C = 20,\n    D,\n}\n",
            "mod_compat",
        ),
        // 値を観測できない設定 (データを持つ variant が有る) でだけ値が変わる
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"payload\")]\n    Data(u8),\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"payload\")]\n    Extra,\n    #[cfg(feature = \"payload\")]\n    Data(u8),\n}\n",
            "mod_compat",
        ),
        // 旧くから `as` を使えない設定でだけ、データを持つ variant を足す
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"a\")]\n    AData(u8),\n    #[cfg(feature = \"b\")]\n    BData(u8),\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"a\")]\n    AData(u8),\n    #[cfg(feature = \"b\")]\n    BData(u8),\n    #[cfg(any(feature = \"a\", feature = \"b\"))]\n    Extra(u8),\n}\n",
            "mod_compat",
        ),
        (
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg_attr(feature = \"a\", non_exhaustive)]\n    Reserved,\n}\n",
            "#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg_attr(feature = \"a\", non_exhaustive)]\n    Reserved,\n    #[cfg(feature = \"a\")]\n    Extra(u8),\n}\n",
            "mod_compat",
        ),
        // 条件付きの `repr(C)` の条件を広げる
        (
            "#[cfg_attr(feature = \"ffi\", repr(C))]\npub enum Kind {\n    Data(u8),\n}\n",
            "#[cfg_attr(any(feature = \"ffi\", feature = \"wire\"), repr(C))]\npub enum Kind {\n    Data(u8),\n}\n",
            "mod_compat",
        ),
        // 同時に有ることの無いフィールドの、宣言の並びだけを入れ替える
        (
            "#[repr(u8)]\npub enum Kind {\n    V {\n        #[cfg(wide)]\n        value: u32,\n        #[cfg(not(wide))]\n        value: u16,\n        flag: u8,\n    },\n}\n",
            "#[repr(u8)]\npub enum Kind {\n    V {\n        #[cfg(not(wide))]\n        value: u16,\n        #[cfg(wide)]\n        value: u32,\n        flag: u8,\n    },\n}\n",
            "none",
        ),
        // どの設定にも有った同じ名前の variant を 1 つにまとめる
        (
            "pub enum Kind {\n    Data(u8),\n    #[cfg(unix)]\n    Ready,\n    #[cfg(not(unix))]\n    Ready,\n}\n",
            "pub enum Kind {\n    Data(u8),\n    Ready,\n}\n",
            "none",
        ),
        // 既定の表現に `repr(C)` を付けるのと一緒に、名前付きのフィールドを並べ替える
        (
            "pub enum Kind {\n    V { x: u8, y: u16 },\n}\n",
            "#[repr(C)]\npub enum Kind {\n    V { y: u16, x: u8 },\n}\n",
            "mod_compat",
        ),
        // 入れ子の `cfg_attr` の内側の条件を変えて、ある設定の `repr(C)` を外す
        (
            "#[cfg_attr(ffi, cfg_attr(wide, repr(C)))]\npub enum Kind {\n    A(u8),\n    B(u16),\n}\n",
            "#[cfg_attr(ffi, cfg_attr(not(wide), repr(C)))]\npub enum Kind {\n    A(u8),\n    B(u16),\n}\n",
            "mod",
        ),
        // 述語が多くても、表現が同じなら末尾への追加は互換
        (
            "#[cfg_attr(target_os = \"linux\", repr(C))]\n#[cfg_attr(target_os = \"windows\", repr(C))]\n#[cfg_attr(target_os = \"macos\", repr(C))]\n#[cfg_attr(target_os = \"android\", repr(C))]\n#[cfg_attr(target_os = \"ios\", repr(C))]\n#[cfg_attr(target_os = \"freebsd\", repr(C))]\n#[cfg_attr(target_os = \"openbsd\", repr(C))]\n#[non_exhaustive]\npub enum Kind {\n    Data(u8),\n}\n",
            "#[cfg_attr(target_os = \"linux\", repr(C))]\n#[cfg_attr(target_os = \"windows\", repr(C))]\n#[cfg_attr(target_os = \"macos\", repr(C))]\n#[cfg_attr(target_os = \"android\", repr(C))]\n#[cfg_attr(target_os = \"ios\", repr(C))]\n#[cfg_attr(target_os = \"freebsd\", repr(C))]\n#[cfg_attr(target_os = \"openbsd\", repr(C))]\n#[non_exhaustive]\npub enum Kind {\n    Data(u8),\n    Extra(u8),\n}\n",
            "mod_compat",
        ),
    ];
    for (old, new, expected) in &cases {
        assert_eq!(bucket(old, new, "Kind"), *expected, "{old}\n=>\n{new}");
    }
}

/// 新しい側だけが同じ名前の重複でビルドできなくなる設定は壊す。enum 自身の cfg、1 つの値しか
/// 取らない鍵、`cfg(true)`・`cfg(false)`、文字列の書き方、推論される寿命の境界も設定ごとに扱う。
#[test]
fn rust_enum_configurations_follow_rust_cfg_semantics() {
    let cases: Vec<(&str, &str, &str)> = vec![
        // feature を有効にした設定で、新しい側だけ同じ名前の variant が 2 つになる
        (
            "pub enum Kind {\n    #[cfg(feature = \"x\")]\n    A(u8),\n    #[cfg(not(feature = \"x\"))]\n    A(u8),\n}\n",
            "pub enum Kind {\n    #[cfg(feature = \"x\")]\n    A(u8),\n    A(u8),\n}\n",
            "mod",
        ),
        // enum 自身が無い設定は比べない
        (
            "#[cfg(feature = \"x\")]\n#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"x\")]\n    Data(u8),\n}\n",
            "#[cfg(feature = \"x\")]\n#[non_exhaustive]\npub enum Kind {\n    A,\n    #[cfg(feature = \"x\")]\n    Data(u8),\n    Extra(u8),\n}\n",
            "mod_compat",
        ),
        // どの設定でも同じ形の寿命を持つ variant をまとめる
        (
            "pub enum Kind<'a, T> {\n    #[cfg(feature = \"x\")]\n    Value(&'a T),\n    #[cfg(not(feature = \"x\"))]\n    Value(&'a T),\n}\n",
            "pub enum Kind<'a, T> {\n    Value(&'a T),\n}\n",
            "none",
        ),
        // `target_os` は 1 つの値しか取らない
        (
            "pub enum Kind {\n    #[cfg(target_os = \"linux\")]\n    Linux,\n    #[cfg(target_os = \"windows\")]\n    Windows,\n    Other,\n}\n",
            "pub enum Kind {\n    #[cfg(target_os = \"windows\")]\n    Windows,\n    #[cfg(target_os = \"linux\")]\n    Linux,\n    Other,\n}\n",
            "none",
        ),
        // `cfg(false)` は定数
        (
            "pub enum Kind {\n    A,\n    #[cfg(false)]\n    Disabled(u8),\n}\n",
            "pub enum Kind {\n    A,\n    #[cfg(false)]\n    Disabled(u16),\n}\n",
            "none",
        ),
        // 文字列の書き方が違っても同じ feature
        (
            "pub enum Kind {\n    A,\n    #[cfg(feature = \"x\")]\n    B,\n}\n",
            "pub enum Kind {\n    A,\n    #[cfg(feature = r\"x\")]\n    B,\n}\n",
            "none",
        ),
    ];
    for (old, new, expected) in &cases {
        assert_eq!(bucket(old, new, "Kind"), *expected, "{old}\n=>\n{new}");
    }
}
