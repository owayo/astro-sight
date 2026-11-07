//! Rust の公開構造体の公開フィールドの変更 (Issue #270)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

/// `src/lib.rs` を `old` で コミットし、`new` に書き換えたときの API の差分。`extra` は
/// 変更しないほかのファイル、`changes` は同じ差分で変えるほかのファイル。
fn api_changes(
    old: &str,
    new: &str,
    extra: &[(&str, &str)],
    changes: &[(&str, &str)],
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut files = vec![("Cargo.toml", CARGO), ("src/lib.rs", old)];
    files.extend_from_slice(extra);
    git_commit_files(repo, &files, "base");
    fs::write(repo.join("src/lib.rs"), new).expect("write");
    for (path, content) in changes {
        fs::write(repo.join(path), content).expect("write");
    }
    detect_api_changes_from_worktree(repo)
}

/// `api_changes` の `name` の区分。
fn bucket(
    old: &str,
    new: &str,
    extra: &[(&str, &str)],
    changes: &[(&str, &str)],
    name: &str,
) -> &'static str {
    let api = api_changes(old, new, extra, changes);
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
        .any(|change| change.name == name && change.reason == "rust_struct_shape_compatible")
    {
        "mod_compat"
    } else {
        "none"
    }
}

/// 期待する区分の表を確かめる。
fn assert_buckets(cases: &[(&str, &str, &str)], name: &str) {
    for (old, new, expected) in cases {
        assert_eq!(
            bucket(old, new, &[], &[], name),
            *expected,
            "{old}\n=>\n{new}"
        );
    }
}

const NOTE: &str = "pub struct Note {\n    pub name: &'static str,\n    pub text: &'static str,\n}\n\npub fn make() -> Note {\n    Note { name: \"a\", text: \"b\" }\n}\n";

/// 公開フィールドの削除・改名・型の変更は、同じ差分で中の構造体式を直しても api.mod
/// (Issue #270 の再現)。
#[test]
fn rust_public_field_changes_are_modified() {
    for new in [
        // 改名 (構造体式も合わせる)
        "pub struct Note {\n    pub name: &'static str,\n    pub body: &'static str,\n}\n\npub fn make() -> Note {\n    Note { name: \"a\", body: \"b\" }\n}\n",
        // 削除
        "pub struct Note {\n    pub name: &'static str,\n}\n\npub fn make() -> Note {\n    Note { name: \"a\" }\n}\n",
        // 型の変更
        "pub struct Note {\n    pub name: &'static str,\n    pub text: String,\n}\n\npub fn make() -> Note {\n    Note { name: \"a\", text: String::new() }\n}\n",
        // 公開でなくする
        "pub struct Note {\n    pub name: &'static str,\n    pub(crate) text: &'static str,\n}\n\npub fn make() -> Note {\n    Note { name: \"a\", text: \"b\" }\n}\n",
        // 外から構築できる構造体への追加 (外の構造体式が足りなくなる)
        "pub struct Note {\n    pub name: &'static str,\n    pub text: &'static str,\n    pub tag: u8,\n}\n\npub fn make() -> Note {\n    Note { name: \"a\", text: \"b\", tag: 0 }\n}\n",
    ] {
        assert_eq!(bucket(NOTE, new, &[], &[], "Note"), "mod", "{new}");
    }
    // signature は公開の形 (名前付きの公開フィールドは名前の順)。
    let api = api_changes(
        NOTE,
        "pub struct Note {\n    pub name: &'static str,\n    pub body: &'static str,\n}\n\npub fn make() -> Note {\n    Note { name: \"a\", body: \"b\" }\n}\n",
        &[],
        &[],
    );
    let change = api
        .modified
        .iter()
        .find(|change| change.name == "Note")
        .expect("Note is modified");
    assert_eq!(
        change.old_signature.as_deref(),
        Some("pub struct Note { pub name: &'static str, pub text: &'static str }")
    );
    assert_eq!(
        change.new_signature.as_deref(),
        Some("pub struct Note { pub body: &'static str, pub name: &'static str }")
    );
    // 1 行の宣言と構造体式でも、中の利用の追随で降格しない。
    let one_line = |field: &str| {
        format!(
            "pub struct Note {{ pub name: &'static str, pub {field}: &'static str }}\npub fn make() -> Note {{ Note {{ name: \"a\", {field}: \"b\" }} }}\n"
        )
    };
    assert_eq!(
        bucket(&one_line("text"), &one_line("body"), &[], &[], "Note"),
        "mod"
    );
    // 別のファイルの利用を同じ差分で直しても同じ。
    let user_old =
        "use crate::Note;\n\npub fn other() -> Note {\n    Note { name: \"x\", text: \"y\" }\n}\n";
    let user_new =
        "use crate::Note;\n\npub fn other() -> Note {\n    Note { name: \"x\", body: \"y\" }\n}\n";
    let lib = |field: &str| {
        format!(
            "pub mod user;\n\npub struct Note {{\n    pub name: &'static str,\n    pub {field}: &'static str,\n}}\n"
        )
    };
    assert_eq!(
        bucket(
            &lib("text"),
            &lib("body"),
            &[("src/user.rs", user_old)],
            &[("src/user.rs", user_new)],
            "Note"
        ),
        "mod"
    );
}

/// 外から構築できなくする変更と、形・属性の変更は api.mod。
#[test]
fn rust_struct_construction_and_shape_changes_are_modified() {
    let all_public = "pub struct Note {\n    pub text: &'static str,\n}\n";
    for new in [
        // 最初の公開でないフィールド
        "pub struct Note {\n    pub text: &'static str,\n    hidden: (),\n}\n",
        // `#[non_exhaustive]` の追加 (`cfg_attr` の中も)
        "#[non_exhaustive]\npub struct Note {\n    pub text: &'static str,\n}\n",
        "#[cfg_attr(feature = \"x\", non_exhaustive)]\npub struct Note {\n    pub text: &'static str,\n}\n",
        // cfg の付いた公開でないフィールド (設定によって構築できなくなる)
        "pub struct Note {\n    pub text: &'static str,\n    #[cfg(feature = \"x\")]\n    hidden: (),\n}\n",
        // 公開フィールドの cfg の付け外し
        "pub struct Note {\n    #[cfg(feature = \"x\")]\n    pub text: &'static str,\n}\n",
        // repr(packed)・repr(align) の追加 (`cfg_attr` の中と `r#` の表記も)
        "#[repr(packed)]\npub struct Note {\n    pub text: &'static str,\n}\n",
        "#[repr(align(8))]\npub struct Note {\n    pub text: &'static str,\n}\n",
        "#[cfg_attr(all(), repr(packed))]\npub struct Note {\n    pub text: &'static str,\n}\n",
        "#[r#repr(packed)]\npub struct Note {\n    pub text: &'static str,\n}\n",
        // 型の引数と公開フィールドの追加
        "pub struct Note<T = ()> {\n    pub text: &'static str,\n    pub extra: T,\n}\n",
    ] {
        assert_eq!(bucket(all_public, new, &[], &[], "Note"), "mod", "{new}");
    }
    assert_buckets(
        &[
            // repr を外す
            (
                "#[repr(C)]\npub struct Note {\n    pub text: &'static str,\n}\n",
                "pub struct Note {\n    pub text: &'static str,\n}\n",
                "mod",
            ),
            // repr の構造体の公開フィールドの並べ替え
            (
                "#[repr(C)]\npub struct Note {\n    pub a: u8,\n    pub b: u16,\n}\n",
                "#[repr(C)]\npub struct Note {\n    pub b: u16,\n    pub a: u8,\n}\n",
                "mod",
            ),
            // マクロの引数は葉の切れ目で比べる (`& &` と `&&` は別のトークン)
            (
                "macro_rules! pick {\n    (& & $t:ty) => { u8 };\n    (&& $t:ty) => { u16 };\n}\npub struct Note {\n    pub value: pick!(& &u8),\n}\n",
                "macro_rules! pick {\n    (& & $t:ty) => { u8 };\n    (&& $t:ty) => { u16 };\n}\npub struct Note {\n    pub value: pick!(&&u8),\n}\n",
                "mod",
            ),
        ],
        "Note",
    );
    // 形の変更 (ユニット → 名前付き)
    assert_eq!(
        bucket(
            "pub struct Unit;\n",
            "pub struct Unit {}\n",
            &[],
            &[],
            "Unit"
        ),
        "mod"
    );
    assert_buckets(
        &[
            // タプルの公開フィールドの位置の移動
            (
                "pub struct Pair(pub u8, ());\n",
                "pub struct Pair((), pub u8, ());\n",
                "mod",
            ),
            // 前の公開でないフィールドの cfg が変わり、公開フィールドの実際の位置が変わる
            (
                "pub struct Pair(#[cfg(any())] (), pub u8, ());\n",
                "pub struct Pair(#[cfg(all())] (), pub u8, ());\n",
                "mod",
            ),
        ],
        "Pair",
    );
}

/// 型の引数と `where` の変更: 強める・既定値を変える・引数を増減するのは api.mod。
#[test]
fn rust_struct_generic_changes_follow_semver() {
    assert_buckets(
        &[
            // 境界を強める
            (
                "pub struct Note<T> {\n    pub value: T,\n}\n",
                "pub struct Note<T: Clone> {\n    pub value: T,\n}\n",
                "mod",
            ),
            (
                "pub struct Note<T: ?Sized> {\n    pub value: Box<T>,\n}\n",
                "pub struct Note<T> {\n    pub value: Box<T>,\n}\n",
                "mod",
            ),
            (
                "pub struct Note<T> {\n    pub value: T,\n}\n",
                "pub struct Note<T> where Vec<T>: Clone {\n    pub value: T,\n}\n",
                "mod",
            ),
            // 既定値の変更と、既定値の無い引数・ライフタイムの追加
            (
                "pub struct Note<T = u8> {\n    pub value: T,\n}\n",
                "pub struct Note<T = u16> {\n    pub value: T,\n}\n",
                "mod",
            ),
            (
                "pub struct Note<T> {\n    pub value: T,\n}\n",
                "pub struct Note<T, U> {\n    pub value: T,\n    marker: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            (
                "pub struct Note {\n    pub value: &'static str,\n}\n",
                "pub struct Note<'a> {\n    pub value: &'a str,\n}\n",
                "mod",
            ),
            // 既定値に置き換えると型が変わる
            (
                "pub struct Note<T = u8> {\n    pub value: T,\n    pub other: u8,\n}\n",
                "pub struct Note<T = u8> {\n    pub value: T,\n    pub other: T,\n}\n",
                "mod",
            ),
            // 境界を緩める・`where` へ移す・既定値を付ける・既定値の付いた引数を足す
            (
                "pub struct Note<T: Clone> {\n    pub value: T,\n}\n",
                "pub struct Note<T> {\n    pub value: T,\n}\n",
                "mod_compat",
            ),
            (
                "pub struct Note<T: Clone + Send> {\n    pub value: T,\n}\n",
                "pub struct Note<T: Send>\nwhere\n    T: Clone,\n{\n    pub value: T,\n}\n",
                "none",
            ),
            (
                "pub struct Note<T> {\n    pub value: T,\n}\n",
                "pub struct Note<T = u8> {\n    pub value: T,\n}\n",
                "mod_compat",
            ),
            (
                "pub struct Note<T> {\n    pub value: T,\n    marker: (),\n}\n",
                "pub struct Note<T, A = ()> {\n    pub value: T,\n    marker: std::marker::PhantomData<A>,\n}\n",
                "mod_compat",
            ),
            // 改名は変更にしない
            (
                "pub struct Note<'a, T> {\n    pub value: &'a T,\n}\n",
                "pub struct Note<'b, U> {\n    pub value: &'b U,\n}\n",
                "none",
            ),
        ],
        "Note",
    );
    // 既定値の付いた引数で一般化する (既定値で置き換えると同じ型)
    assert_buckets(
        &[
            (
                "pub struct Id(pub u8);\n",
                "pub struct Id<T = u8>(pub T);\n",
                "mod_compat",
            ),
            (
                "pub struct Id<T>(pub T, pub T);\n",
                "pub struct Id<T, U = T>(pub T, pub U);\n",
                "mod_compat",
            ),
        ],
        "Id",
    );
    // ユニット構造体の `where` (文法が ERROR に包む) も形として読む (緩めるのは互換)。
    assert_buckets(
        &[
            (
                "pub trait Allowed {}\npub struct Note<const N: usize>\nwhere\n    [(); N]: Sized;\n",
                "pub trait Allowed {}\npub struct Note<const N: usize>\nwhere\n    [(); N]: Allowed;\n",
                "mod",
            ),
            (
                "pub trait Allowed {}\npub struct Note<const N: usize>\nwhere\n    [(); N]: Sized,\n    [(); N]: Allowed;\n",
                "pub trait Allowed {}\npub struct Note<const N: usize>\nwhere\n    [(); N]: Sized;\n",
                "mod_compat",
            ),
        ],
        "Note",
    );
    // 形を読めない構造体 (型の引数の属性) は、宣言全体を signature にして blocking に残す。
    assert_buckets(
        &[(
            "pub struct Note<#[cfg(all())] T> {\n    pub text: T,\n}\n",
            "pub struct Note<#[cfg(all())] T> {\n    pub body: T,\n}\n",
            "mod",
        )],
        "Note",
    );
}

/// 外から見えない変更は出さず、制限を緩める・広げるだけの変更は互換 (対照)。
#[test]
fn rust_struct_private_and_relaxing_changes_are_not_blocking() {
    let restricted = "pub struct Note {\n    pub text: &'static str,\n    hidden: (),\n}\n";
    for (new, expected) in [
        // 公開でないフィールドの型の変更・追加 (すでに構築できない)
        (
            "pub struct Note {\n    pub text: &'static str,\n    hidden: u8,\n    more: bool,\n}\n",
            "none",
        ),
        // 文書と整形だけ
        (
            "/// A note.\npub struct Note {\n    /// The text.\n    pub text:   &'static   str,\n    hidden: (),\n}\n",
            "none",
        ),
        // 最後の公開でないフィールドを外す (構築できるようになる)
        (
            "pub struct Note {\n    pub text: &'static str,\n}\n",
            "mod_compat",
        ),
        // 名前付きのフィールドの並べ替え (repr が無い)
        (
            "pub struct Note {\n    hidden: (),\n    pub text: &'static str,\n}\n",
            "none",
        ),
        // 構築できない構造体への公開フィールドの追加
        (
            "pub struct Note {\n    pub text: &'static str,\n    pub tag: u8,\n    hidden: (),\n}\n",
            "mod_compat",
        ),
        // `r#` の表記と、文書のための `cfg_attr`
        (
            "pub struct Note {\n    pub r#text: &'static str,\n    hidden: (),\n}\n",
            "none",
        ),
        (
            "pub struct Note {\n    #[cfg_attr(docsrs, doc(cfg(feature = \"x\")))]\n    pub text: &'static str,\n    hidden: (),\n}\n",
            "none",
        ),
        // 型の表記の空白
        (
            "pub struct Note {\n    pub text: & 'static str,\n    hidden: (),\n}\n",
            "none",
        ),
    ] {
        assert_eq!(bucket(restricted, new, &[], &[], "Note"), expected, "{new}");
    }
    assert_buckets(
        &[
            // `#[non_exhaustive]` を外す (`cfg_attr` の中も)
            (
                "#[non_exhaustive]\npub struct Note {\n    pub text: &'static str,\n}\n",
                "pub struct Note {\n    pub text: &'static str,\n}\n",
                "mod_compat",
            ),
            (
                "#[cfg_attr(feature = \"x\", non_exhaustive)]\npub struct Note {\n    pub text: &'static str,\n}\n",
                "pub struct Note {\n    pub text: &'static str,\n}\n",
                "mod_compat",
            ),
            // 形に効かない属性 (文書のための `cfg_attr`・derive)
            (
                "pub struct Note {\n    pub text: &'static str,\n}\n",
                "#[cfg_attr(docsrs, doc(cfg(feature = \"x\")))]\n#[derive(Clone)]\npub struct Note {\n    pub text: &'static str,\n}\n",
                "none",
            ),
            // 既定の表現に repr(C)・repr(transparent) を付ける
            (
                "pub struct Note {\n    pub text: &'static str,\n}\n",
                "#[repr(C)]\npub struct Note {\n    pub text: &'static str,\n}\n",
                "mod_compat",
            ),
            (
                "pub struct Note {\n    pub text: &'static str,\n}\n",
                "#[repr(transparent)]\npub struct Note {\n    pub text: &'static str,\n}\n",
                "mod_compat",
            ),
            // repr(C) の公開でないフィールドの変更と移動 (Cargo の手引きでも互換)
            (
                "#[repr(C)]\npub struct Note {\n    hidden: u8,\n    pub value: u32,\n}\n",
                "#[repr(C)]\npub struct Note {\n    pub value: u32,\n    hidden: u16,\n}\n",
                "none",
            ),
        ],
        "Note",
    );
    assert_buckets(
        &[
            // タプルの末尾に公開でないフィールドを足す (公開の位置は変わらない)
            (
                "pub struct Pair(pub u8, ());\n",
                "pub struct Pair(pub u8, (), bool);\n",
                "none",
            ),
            // 公開フィールドの無いタプル ↔ 名前付き
            (
                "pub struct Pair(u8, u16);\n",
                "pub struct Pair {\n    first: u8,\n    second: u16,\n}\n",
                "mod_compat",
            ),
        ],
        "Pair",
    );
}

/// signature は形に効く属性 (`cfg_attr` の中と `r#` の表記も)・cfg の付いた位置・葉の切れ目を示す。
#[test]
fn rust_struct_signatures_show_the_public_shape() {
    let new_signature = |old: &str, new: &str, name: &str| {
        api_changes(old, new, &[], &[])
            .modified
            .iter()
            .find(|change| change.name == name)
            .and_then(|change| change.new_signature.clone())
    };
    assert_eq!(
        new_signature(
            "pub struct Note {\n    pub text: u8,\n}\n",
            "#[r#repr(C)]\n#[cfg_attr(all(), repr(packed), derive(Debug))]\n#[derive(Clone)]\npub struct Note {\n    pub text: u8,\n}\n",
            "Note"
        )
        .as_deref(),
        Some("#[r#repr(C)] #[cfg_attr(all(), repr(packed))] pub struct Note { pub text: u8 }")
    );
    assert_eq!(
        new_signature(
            "pub struct Pair(#[cfg(any())] (), pub u8, ());\n",
            "pub struct Pair(#[cfg(all())] (), pub u8, ());\n",
            "Pair"
        )
        .as_deref(),
        Some("pub struct Pair(#[cfg(all())] _, pub u8, ..);")
    );
    let picked = |argument: &str| {
        format!(
            "macro_rules! pick {{\n    (& & $t:ty) => {{ u8 }};\n    (&& $t:ty) => {{ u16 }};\n}}\npub struct Note {{\n    pub value: pick!({argument}),\n}}\n"
        )
    };
    let api = api_changes(&picked("& &u8"), &picked("&&u8"), &[], &[]);
    let change = api
        .modified
        .iter()
        .find(|change| change.name == "Note")
        .expect("Note is modified");
    assert_eq!(
        change.old_signature.as_deref(),
        Some("pub struct Note { pub value: pick!(& &u8) }")
    );
    assert_eq!(
        change.new_signature.as_deref(),
        Some("pub struct Note { pub value: pick!(&&u8) }")
    );
}

/// 公開面: re-export した非公開の module の型は対象、外へ出ない module の型は対象外。
#[test]
fn rust_struct_field_changes_follow_the_public_surface() {
    let detail = |field: &str| format!("pub struct Note {{\n    pub {field}: &'static str,\n}}\n");
    let old = detail("text");
    let new = detail("body");
    for (lib, expected) in [
        ("mod detail;\npub use detail::Note;\n", "mod"),
        ("mod detail;\n", "none"),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", CARGO),
                ("src/lib.rs", lib),
                ("src/detail.rs", &old),
            ],
            "base",
        );
        fs::write(repo.join("src/detail.rs"), &new).expect("write");
        let api = detect_api_changes_from_worktree(repo);
        let found = if api.modified.iter().any(|change| change.name == "Note") {
            "mod"
        } else {
            "none"
        };
        assert_eq!(found, expected, "{lib}");
    }
}

/// 足した型の引数の境界・暗黙の `Sized` と、寿命の境界 (利用側の暗黙の境界になる)。
#[test]
fn rust_struct_new_parameters_and_outlives_bounds() {
    assert_buckets(
        &[
            // 足した既定値付きの引数の境界は、既定値が満たすとは言えない
            (
                "pub struct S<T> {\n    pub value: T,\n    marker: (),\n}\n",
                "pub struct S<T, U: Clone = T> {\n    pub value: T,\n    marker: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            // `?Sized` だった引数を既定値にすると、足した引数の暗黙の `Sized` を満たさない
            (
                "pub struct S<T: ?Sized> {\n    pub value: Box<T>,\n    marker: (),\n}\n",
                "pub struct S<T: ?Sized, U = T> {\n    pub value: Box<T>,\n    marker: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            // 寿命の境界を外す・足す
            (
                "pub struct S<'a, 'b: 'a>(std::marker::PhantomData<(&'a (), &'b ())>);\n",
                "pub struct S<'a, 'b>(std::marker::PhantomData<(&'a (), &'b ())>);\n",
                "mod",
            ),
            (
                "pub struct S<'a, T: 'a> {\n    pub value: std::marker::PhantomData<&'a ()>,\n    pub item: Box<T>,\n}\n",
                "pub struct S<'a, T> {\n    pub value: std::marker::PhantomData<&'a ()>,\n    pub item: Box<T>,\n}\n",
                "mod",
            ),
            (
                "pub struct S<'a, T> {\n    pub value: std::marker::PhantomData<&'a ()>,\n    pub item: Box<T>,\n}\n",
                "pub struct S<'a, T> where T: 'a {\n    pub value: std::marker::PhantomData<&'a ()>,\n    pub item: Box<T>,\n}\n",
                "mod",
            ),
            // フィールドの参照から推論される寿命の境界は、書いても書かなくても同じ
            (
                "pub struct S<'a, T: 'a> {\n    pub value: &'a T,\n}\n",
                "pub struct S<'a, T> {\n    pub value: &'a T,\n}\n",
                "none",
            ),
            (
                "pub struct S<'a, 'b: 'a, T> {\n    pub value: &'a &'b T,\n}\n",
                "pub struct S<'a, 'b, T> where T: 'b {\n    pub value: &'a &'b T,\n}\n",
                "none",
            ),
            // 独自の `Sized` トレイトの境界は、暗黙の `Sized` と別
            (
                "pub trait Sized {}\npub struct S<T> {\n    pub value: T,\n}\n",
                "pub trait Sized {}\npub struct S<T: Sized> {\n    pub value: T,\n}\n",
                "mod",
            ),
            // crate の根からの標準の `Sized` を明示するだけ、`for<'a>` の束縛の改名
            (
                "pub struct S<T> {\n    pub value: T,\n}\n",
                "pub struct S<T: ::std::marker::Sized> {\n    pub value: T,\n}\n",
                "none",
            ),
            // 相対の `std::marker::Sized` は同じ名前の独自のモジュールかもしれない
            (
                "pub mod std {\n    pub mod marker {\n        pub trait Sized {}\n    }\n}\npub struct S<T> {\n    pub value: T,\n}\n",
                "pub mod std {\n    pub mod marker {\n        pub trait Sized {}\n    }\n}\npub struct S<T: std::marker::Sized> {\n    pub value: T,\n}\n",
                "mod",
            ),
            // 足した引数の既定値が関連型の射影・`?Sized` の引数を経る・名前の付いた型なら
            // `Sized` と言えない (`Box<T>` も構文からは言わない)
            (
                "pub trait Family {\n    type Out: ?Sized;\n}\npub struct S<T: Family> {\n    pub value: Box<T::Out>,\n    hidden: std::marker::PhantomData<T>,\n}\n",
                "pub trait Family {\n    type Out: ?Sized;\n}\npub struct S<T: Family, U = <T as Family>::Out> {\n    pub value: Box<T::Out>,\n    hidden: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            (
                "pub struct S<T: ?Sized> {\n    pub value: Box<T>,\n    hidden: (),\n}\n",
                "pub struct S<T: ?Sized, U: ?Sized = T, V = U> {\n    pub value: Box<T>,\n    hidden: std::marker::PhantomData<(Box<U>, V)>,\n}\n",
                "mod",
            ),
            (
                "pub struct S<T: ?Sized> {\n    pub value: Box<T>,\n    hidden: (),\n}\n",
                "pub struct S<T: ?Sized, U = Box<T>> {\n    pub value: Box<T>,\n    hidden: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            // 引数を含まない参照・基本型・タプル・配列の既定値は `Sized` で、どの利用でも正しい型
            (
                "pub struct S<T: ?Sized> {\n    pub value: Box<T>,\n    hidden: (),\n}\n",
                "pub struct S<T: ?Sized, U = (&'static str, [u8; 4])> {\n    pub value: Box<T>,\n    hidden: std::marker::PhantomData<U>,\n}\n",
                "mod_compat",
            ),
            // 引数を参照する既定値は寿命の境界を求める (`&'static T` は `T: 'static`)
            (
                "pub struct S<T: ?Sized> {\n    pub value: Box<T>,\n    hidden: (),\n}\n",
                "pub struct S<T: ?Sized, U = &'static T> {\n    pub value: Box<T>,\n    hidden: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            (
                "pub struct S {\n    pub f: for<'a> fn(&'a str) -> &'a str,\n}\n",
                "pub struct S {\n    pub f: for<'b> fn(&'b str) -> &'b str,\n}\n",
                "none",
            ),
        ],
        "S",
    );
}

/// 改名の置き換えは名前空間と関連型の名前を区別し、マクロの引数は字面で比べる。
#[test]
fn rust_struct_names_follow_namespaces_and_macro_tokens() {
    assert_buckets(
        &[
            // フィールドの型の `A` は型の名前空間の構造体で、定数の引数ではない
            (
                "pub struct A;\npub struct B;\npub struct S<const A: usize> {\n    pub value: A,\n}\n",
                "pub struct A;\npub struct B;\npub struct S<const B: usize> {\n    pub value: B,\n}\n",
                "mod",
            ),
            // 関連型の束縛と境界の名前は型の引数ではない
            (
                "pub trait Values {\n    type Item;\n    type Output;\n}\npub struct S<Item, T: Values<Item: Copy>> {\n    pub value: T,\n    pub item: Item,\n}\n",
                "pub trait Values {\n    type Item;\n    type Output;\n}\npub struct S<Output, T: Values<Output: Copy>> {\n    pub value: T,\n    pub item: Output,\n}\n",
                "mod",
            ),
            // マクロの名前は定数の引数ではない
            (
                "macro_rules! A {\n    () => { u8 };\n}\nmacro_rules! B {\n    () => { u16 };\n}\npub struct S<const A: usize> {\n    pub value: A!(),\n    hidden: (),\n}\n",
                "macro_rules! A {\n    () => { u8 };\n}\nmacro_rules! B {\n    () => { u16 };\n}\npub struct S<const B: usize> {\n    pub value: B!(),\n    hidden: (),\n}\n",
                "mod",
            ),
            // マクロの入力のドキュメントコメントは、末尾の空白も文字列に入る
            (
                "macro_rules! pick {\n    (#[doc = $s:literal]) => { [u8; $s.len()] };\n}\npub struct S {\n    pub value: pick!(/// x \n    ),\n}\n",
                "macro_rules! pick {\n    (#[doc = $s:literal]) => { [u8; $s.len()] };\n}\npub struct S {\n    pub value: pick!(/// x\n    ),\n}\n",
                "mod",
            ),
            // 関連型の束縛の名前は型の引数ではない
            (
                "pub trait Values {\n    type Item;\n    type Output;\n}\npub struct S<Item, T: Values<Item = u8>> {\n    pub value: T,\n    pub item: Item,\n}\n",
                "pub trait Values {\n    type Item;\n    type Output;\n}\npub struct S<Output, T: Values<Output = u8>> {\n    pub value: T,\n    pub item: Output,\n}\n",
                "mod",
            ),
            // マクロは `r#T` と `T`、ドキュメントコメントと普通のコメントを区別する
            (
                "macro_rules! pick {\n    (r#T) => { u8 };\n    (T) => { u16 };\n}\npub struct S {\n    pub value: pick!(r#T),\n}\n",
                "macro_rules! pick {\n    (r#T) => { u8 };\n    (T) => { u16 };\n}\npub struct S {\n    pub value: pick!(T),\n}\n",
                "mod",
            ),
            (
                "macro_rules! pick {\n    (#[doc = $s:literal] u8) => { u8 };\n    (u8) => { u16 };\n}\npub struct S {\n    pub value: pick!(/** text */ u8),\n}\n",
                "macro_rules! pick {\n    (#[doc = $s:literal] u8) => { u8 };\n    (u8) => { u16 };\n}\npub struct S {\n    pub value: pick!(/* text */ u8),\n}\n",
                "mod",
            ),
            // 形を読めない構造体の signature は、文字列の中の空白を潰さない
            (
                "pub struct S<#[cfg(all())] T> {\n    pub value: [u8; \"a  b\".len()],\n    marker: std::marker::PhantomData<T>,\n}\n",
                "pub struct S<#[cfg(all())] T> {\n    pub value: [u8; \"a b\".len()],\n    marker: std::marker::PhantomData<T>,\n}\n",
                "mod",
            ),
        ],
        "S",
    );
}

/// `#[repr]` は項目の集合で比べ、足す公開フィールドは旧い公開フィールドの後に置く。
#[test]
fn rust_struct_repr_items_and_public_additions() {
    assert_buckets(
        &[
            (
                "#[repr(C)]\npub struct S {\n    pub value: u32,\n}\n",
                "#[repr(C,)]\npub struct S {\n    pub value: u32,\n}\n",
                "none",
            ),
            (
                "#[repr(C)]\n#[repr(align(8))]\npub struct S {\n    pub value: u32,\n}\n",
                "#[repr(align(8), C)]\npub struct S {\n    pub value: u32,\n}\n",
                "none",
            ),
            // `packed` と `packed(1)`、`align(0x8)` と `align(8)` は同じ (`cfg_attr` の中も。
            // 属性の引数の中の `r#repr` も `repr`)
            (
                "#[repr(C, packed)]\npub struct S {\n    pub value: u32,\n}\n",
                "#[repr(C, packed(1,))]\npub struct S {\n    pub value: u32,\n}\n",
                "none",
            ),
            (
                "#[cfg_attr(all(), repr(C, packed))]\npub struct S {\n    pub value: u32,\n}\n",
                "#[cfg_attr(all(), r#repr(packed(1,), C))]\npub struct S {\n    pub value: u32,\n}\n",
                "none",
            ),
            (
                "#[cfg_attr(all(), repr(C, align(0x8)))]\npub struct S {\n    pub value: u32,\n}\n",
                "#[cfg_attr(all(), r#repr(align(8,), C))]\npub struct S {\n    pub value: u32,\n}\n",
                "none",
            ),
            // 複数の `align` は最大のものが効き、`repr(Rust)` は既定の表現
            (
                "#[repr(C, align(4), align(8))]\npub struct S {\n    pub value: u32,\n}\n",
                "#[repr(C, align(8))]\npub struct S {\n    pub value: u32,\n}\n",
                "none",
            ),
            (
                "pub struct S {\n    pub value: u32,\n    pub other: u8,\n}\n",
                "#[repr(Rust)]\npub struct S {\n    pub other: u8,\n    pub value: u32,\n}\n",
                "none",
            ),
            // `cfg_attr` の条件の `r#true` は名前の付いた cfg で、定数の `true` ではない
            (
                "#[cfg_attr(true, repr(C, packed))]\npub struct S {\n    pub value: u32,\n}\n",
                "#[cfg_attr(r#true, repr(C, packed))]\npub struct S {\n    pub value: u32,\n}\n",
                "mod",
            ),
            // 表現は設定ごとに効く項目で比べる (条件の `r#feature` は `feature`、条件付きの小さい
            // `align` は無条件の大きい `align` に負ける)
            (
                "#[cfg_attr(feature = \"ffi\", repr(C))]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "#[cfg_attr(r#feature = \"ffi\", repr(C))]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "none",
            ),
            (
                "#[repr(C, align(8))]\n#[cfg_attr(feature = \"extra\", repr(align(4)))]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "#[repr(C, align(8))]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "none",
            ),
            // 条件付きの `repr(Rust)` は既定の表現なので、フィールドの順を約束しない
            (
                "#[cfg_attr(all(), repr(Rust))]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "#[cfg_attr(all(), repr(Rust))]\npub struct S {\n    pub b: u32,\n    pub a: u8,\n}\n",
                "none",
            ),
            // `#[repr]` が効く設定があれば、順は約束 (`repr(transparent)` で `#[cfg]` で分けた同じ名前の
            // 公開フィールドの片方を消す変更も、順の違いで捉える)
            (
                "#[repr(transparent)]\npub struct S {\n    #[cfg(feature = \"wide\")]\n    pub value: u64,\n    #[cfg(not(feature = \"wide\"))]\n    pub value: u8,\n    hidden: (),\n}\n",
                "#[repr(transparent)]\npub struct S {\n    #[cfg(not(feature = \"wide\"))]\n    pub value: u8,\n    hidden: (),\n}\n",
                "mod",
            ),
            (
                "#[cfg_attr(feature = \"ffi\", repr(C))]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "#[cfg_attr(feature = \"ffi\", repr(C))]\npub struct S {\n    pub b: u32,\n    pub a: u8,\n}\n",
                "mod",
            ),
            // 条件付きの `repr(C)` を無条件にするのは、既定の表現だった設定に足すだけ
            (
                "#[cfg_attr(feature = \"ffi\", repr(C))]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "#[repr(C)]\npub struct S {\n    pub a: u8,\n    pub b: u32,\n}\n",
                "mod_compat",
            ),
            (
                "#[cfg_attr(all(), repr(C, packed))]\npub struct S {\n    pub value: u32,\n}\n",
                "#[cfg_attr(all(), repr(C))]\npub struct S {\n    pub value: u32,\n}\n",
                "mod",
            ),
            // 旧い公開フィールドより前に足すと、そのオフセットが変わる
            (
                "#[repr(C)]\npub struct S {\n    pub value: u32,\n    hidden: (),\n}\n",
                "#[repr(C)]\npub struct S {\n    pub added: u8,\n    pub value: u32,\n    hidden: (),\n}\n",
                "mod",
            ),
            (
                "#[repr(C)]\npub struct S {\n    pub value: u32,\n    hidden: (),\n}\n",
                "#[repr(C)]\npub struct S {\n    pub value: u32,\n    pub added: u8,\n    hidden: (),\n}\n",
                "mod_compat",
            ),
        ],
        "S",
    );
}

/// 寿命の境界: `?Sized` の引数に書いた境界は `dyn` の既定の寿命を決め、推論は cfg の付かない
/// フィールドの型の部分だけから行い、公開でないフィールドから推論される境界も signature に示す。
#[test]
fn rust_struct_outlives_bounds_follow_object_defaults_and_fields() {
    assert_buckets(
        &[
            // `S<'a, dyn Display>` の `dyn` の寿命が `'a` から `'static` に変わる
            (
                "pub struct S<'a, T: ?Sized + 'a> {\n    pub value: &'a T,\n}\n",
                "pub struct S<'a, T: ?Sized> {\n    pub value: &'a T,\n}\n",
                "mod",
            ),
            // cfg の付いたフィールドからは推論しない
            (
                "pub struct S<'a, T: 'a> {\n    #[cfg(any())]\n    hidden: &'a T,\n    marker: std::marker::PhantomData<(&'a (), T)>,\n}\n",
                "pub struct S<'a, T> {\n    #[cfg(any())]\n    hidden: &'a T,\n    marker: std::marker::PhantomData<(&'a (), T)>,\n}\n",
                "mod",
            ),
            // 配列の長さの式の中の名前は構造体の引数ではない
            (
                "pub struct S<'a, T: 'a> {\n    hidden: &'a [u8; { fn local<T>() {} 0 }],\n    marker: std::marker::PhantomData<T>,\n}\n",
                "pub struct S<'a, T> {\n    hidden: &'a [u8; { fn local<T>() {} 0 }],\n    marker: std::marker::PhantomData<T>,\n}\n",
                "mod",
            ),
            // 公開でないフィールドの変更で、推論される境界が無くなる
            (
                "pub struct S<'a, T> {\n    hidden: &'a T,\n}\n",
                "pub struct S<'a, T> {\n    hidden: std::marker::PhantomData<(&'a (), T)>,\n}\n",
                "mod",
            ),
            // 定数の引数は寿命の境界の成分にならない (改名は変更なし)
            (
                "pub struct Buffer<const N: usize>([u8; N]);\npub struct S<'a, const N: usize> {\n    hidden: &'a Buffer<N>,\n}\n",
                "pub struct Buffer<const N: usize>([u8; N]);\npub struct S<'a, const M: usize> {\n    hidden: &'a Buffer<M>,\n}\n",
                "none",
            ),
            // 足した引数の既定値が引数を含まなければ、旧い利用で推論される境界はいつも満たす
            (
                "pub struct S<'a> {\n    pub value: &'a u8,\n}\n",
                "pub struct S<'a, T = u8> {\n    pub value: &'a T,\n}\n",
                "mod_compat",
            ),
            // `?Sized` と標準の `Sized` を両方書けば `Sized` を求める
            (
                "pub struct S<T: ?Sized> {\n    pub value: Box<T>,\n}\n",
                "pub struct S<T: ?Sized + ::core::marker::Sized> {\n    pub value: Box<T>,\n}\n",
                "mod",
            ),
        ],
        "S",
    );
}

/// 足した引数の既定値の妥当性と、関連型・マクロの名前の取りこぼし。
#[test]
fn rust_struct_new_defaults_and_names_are_checked_strictly() {
    assert_buckets(
        &[
            // `?Sized` の引数でも、既定値は旧い利用で正しい型でなければならない
            (
                "pub struct S<T> {\n    pub value: T,\n    hidden: (),\n}\n",
                "pub struct S<T, U: ?Sized = &'static T> {\n    pub value: T,\n    hidden: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            // マクロを含む既定値は、展開した先で引数を使えるので言えない
            (
                "macro_rules! arg {\n    () => { &'static T };\n}\npub struct S<T> {\n    pub value: T,\n    hidden: (),\n}\n",
                "macro_rules! arg {\n    () => { &'static T };\n}\npub struct S<T, U = fn(arg!())> {\n    pub value: T,\n    hidden: std::marker::PhantomData<U>,\n}\n",
                "mod",
            ),
            // GAT の関連型の境界の名前は型の引数ではない
            (
                "pub trait Values {\n    type Item<'a>;\n    type Output<'a>;\n}\npub struct S<Item, T: Values<Item<'static>: Copy>> {\n    pub value: T,\n    pub item: Item,\n}\n",
                "pub trait Values {\n    type Item<'a>;\n    type Output<'a>;\n}\npub struct S<Output, T: Values<Output<'static>: Copy>> {\n    pub value: T,\n    pub item: Output,\n}\n",
                "mod",
            ),
            // 修飾したマクロの名前の先頭は型の引数ではない
            (
                "mod A {\n    macro_rules! m { () => { u8 }; }\n    pub(crate) use m;\n}\nmod B {\n    macro_rules! m { () => { u16 }; }\n    pub(crate) use m;\n}\npub struct S<A> {\n    pub value: A,\n    pub generated: A::m!(),\n}\n",
                "mod A {\n    macro_rules! m { () => { u8 }; }\n    pub(crate) use m;\n}\nmod B {\n    macro_rules! m { () => { u16 }; }\n    pub(crate) use m;\n}\npub struct S<B> {\n    pub value: B,\n    pub generated: B::m!(),\n}\n",
                "mod",
            ),
        ],
        "S",
    );
}

/// 推論は組み込みの型だけをたどり (型の別名は射影かもしれない)、cfg の付いたフィールドからの境界は
/// 条件付きで比べ、自明な `'a: 'a` は数えない。
#[test]
fn rust_struct_inferred_outlives_stay_sound() {
    assert_buckets(
        &[
            // 射影を包む型の別名の中の `T` は、`T: 'a` の根拠にならない
            (
                "pub trait Family {\n    type Item;\n}\npub type ItemOf<T> = <T as Family>::Item;\npub struct S<'a, T: Family + 'a> {\n    hidden: &'a ItemOf<T>,\n}\n",
                "pub trait Family {\n    type Item;\n}\npub type ItemOf<T> = <T as Family>::Item;\npub struct S<'a, T: Family> {\n    hidden: &'a ItemOf<T>,\n}\n",
                "mod",
            ),
            // cfg の付いた公開でないフィールドの変更で、その設定での暗黙の境界が無くなる
            (
                "pub struct S<'a, T> {\n    #[cfg(feature = \"borrowed\")]\n    hidden: &'a T,\n    marker: std::marker::PhantomData<(&'a (), T)>,\n}\n",
                "pub struct S<'a, T> {\n    #[cfg(feature = \"borrowed\")]\n    hidden: std::marker::PhantomData<T>,\n    marker: std::marker::PhantomData<(&'a (), T)>,\n}\n",
                "mod",
            ),
            // `&'a [&'a str]` の `'a: 'a` は自明
            (
                "pub struct S<'a> {\n    hidden: &'a str,\n}\n",
                "pub struct S<'a> {\n    hidden: &'a [&'a str],\n}\n",
                "none",
            ),
        ],
        "S",
    );
}

/// cfg の付いたフィールドからの暗黙の境界が、引数に書いた境界と同じなら、そのフィールドの変更は
/// 寿命の契約を変えない。
#[test]
fn rust_struct_conditional_outlives_written_on_parameters_are_unchanged() {
    assert_buckets(
        &[(
            "pub struct S<'a, T: 'a> {\n    #[cfg(feature = \"borrowed\")]\n    hidden: &'a T,\n    marker: std::marker::PhantomData<(&'a (), T)>,\n}\n",
            "pub struct S<'a, T: 'a> {\n    #[cfg(feature = \"borrowed\")]\n    hidden: std::marker::PhantomData<T>,\n    marker: std::marker::PhantomData<(&'a (), T)>,\n}\n",
            "none",
        )],
        "S",
    );
}
