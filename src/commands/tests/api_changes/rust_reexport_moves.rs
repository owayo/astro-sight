//! Rust で項目を別のファイルへ移し、元のファイルから `pub use` で再公開した差分 (Issue #281)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

/// `old` をコミットし、`new` に書き換えた (中身が None なら消した) ときの API の差分。
fn api_changes(
    old: &[(&str, &str)],
    new: &[(&str, Option<&str>)],
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut files = vec![("Cargo.toml", CARGO)];
    files.extend_from_slice(old);
    git_commit_files(repo, &files, "base");
    for (path, content) in new {
        let full = repo.join(path);
        match content {
            Some(content) => {
                fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
                fs::write(full, content).expect("write");
            }
            None => fs::remove_file(full).expect("remove"),
        }
    }
    detect_api_changes_from_worktree(repo)
}

/// `name` の区分 (どこにも無ければ "none")。
fn bucket(api: &crate::models::review::ApiChanges, name: &str) -> &'static str {
    if api.modified.iter().any(|change| change.name == name) {
        "mod"
    } else if api
        .compatible_modified
        .iter()
        .any(|change| change.name == name)
    {
        "mod_compat"
    } else if api
        .const_value_changes
        .iter()
        .any(|change| change.name == name)
    {
        "const_value"
    } else if api.removed.iter().any(|symbol| symbol.name == name)
        || api.removed_dead.iter().any(|symbol| symbol.name == name)
    {
        "rm"
    } else if api.added.iter().any(|symbol| symbol.name == name) {
        "add"
    } else {
        "none"
    }
}

/// `name` が `moved` にあるか。
fn moved(api: &crate::models::review::ApiChanges, name: &str) -> bool {
    api.moved.iter().any(|symbol| symbol.name == name)
}

/// 元のファイル `src/lib.rs` の `old` を `src/detail.rs` の `moved` へ移し、`lib.rs` を
/// `mod detail; pub use detail::<name>;` にしたときの API の差分。
fn reexported_move(old: &str, moved: &str, name: &str) -> crate::models::review::ApiChanges {
    let lib = format!("mod detail;\npub use detail::{name};\n");
    api_changes(
        &[("src/lib.rs", old)],
        &[("src/lib.rs", Some(&lib)), ("src/detail.rs", Some(moved))],
    )
}

/// 移すのと同じ差分でシグネチャやフィールドを変えると、外の利用者からは同じパスの API の変更
/// なので api.mod (Issue #281 の再現)。移動先の追加は出さない。
#[test]
fn reexported_moves_with_changes_are_modified() {
    for (old, new, name) in [
        (
            "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n",
            "pub fn greet(name: &str, loud: bool) -> String {\n    if loud { name.to_uppercase() } else { name.to_owned() }\n}\n",
            "greet",
        ),
        (
            "pub struct Note {\n    pub text: u8,\n}\n",
            "pub struct Note {\n    pub body: u8,\n}\n",
            "Note",
        ),
        (
            "pub enum Kind {\n    A,\n    B,\n}\n",
            "pub enum Kind {\n    A,\n}\n",
            "Kind",
        ),
        (
            "pub union Value {\n    pub number: u32,\n}\n",
            "pub union Value {\n    pub float: f32,\n}\n",
            "Value",
        ),
    ] {
        let api = reexported_move(old, new, name);
        assert_eq!(bucket(&api, name), "mod", "{name}: {api:?}");
        assert!(
            !api.added.iter().any(|symbol| symbol.name == name),
            "{name}: {api:?}"
        );
    }
}

/// 変えずに移すのは移動。互換な変更と値だけの変更は、移さない場合と同じ区分に出して移動にもする。
#[test]
fn reexported_moves_without_breaking_changes_are_moves() {
    let api = reexported_move(
        "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n",
        "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n",
        "greet",
    );
    assert!(moved(&api, "greet"), "{api:?}");
    assert_eq!(bucket(&api, "greet"), "none", "{api:?}");
    // 外から構築できない構造体への公開フィールドの追加は互換
    let api = reexported_move(
        "pub struct Note {\n    pub text: u8,\n    hidden: u8,\n}\n",
        "pub struct Note {\n    pub text: u8,\n    pub extra: u8,\n    hidden: u8,\n}\n",
        "Note",
    );
    assert!(moved(&api, "Note"), "{api:?}");
    assert_eq!(bucket(&api, "Note"), "mod_compat", "{api:?}");
}

/// 再公開の先が差分の外からある宣言なら、関数はシグネチャで比べ、構造体は型の同一性が変わるので
/// api.mod。
#[test]
fn reexports_of_existing_declarations_follow_their_kind() {
    let old_util = "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n\npub struct Note {\n    pub text: u8,\n}\n";
    let old_lib = "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n\npub struct Note {\n    pub text: u8,\n}\n\npub mod util;\n";
    let api = api_changes(
        &[("src/lib.rs", old_lib), ("src/util.rs", old_util)],
        &[(
            "src/lib.rs",
            Some("pub mod util;\npub use util::{greet, Note};\n"),
        )],
    );
    // 同じシグネチャの関数は変更なし、同じ形でも別の型だった構造体は api.mod
    assert_eq!(bucket(&api, "greet"), "none", "{api:?}");
    assert_eq!(bucket(&api, "Note"), "mod", "{api:?}");
    // 先の関数のシグネチャが違えば api.mod
    let api = api_changes(
        &[
            ("src/lib.rs", old_lib),
            (
                "src/util.rs",
                "pub fn greet(name: &str, loud: bool) -> String {\n    let _ = loud;\n    name.to_owned()\n}\n\npub struct Note {\n    pub text: u8,\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some(
                "pub mod util;\npub use util::greet;\n\npub struct Note {\n    pub text: u8,\n}\n",
            ),
        )],
    );
    assert_eq!(bucket(&api, "greet"), "mod", "{api:?}");
}

/// 再公開の先を経路から確かめられないもの (外部 crate・条件付きの再公開・`as` の改名) は、
/// 対応を確かめられないので blocking な api.mod (`reexport_target_unresolved`) に残す。
#[test]
fn unresolved_reexports_stay_modified() {
    let old = "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n";
    let detail = "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n";
    for (lib, detail) in [
        ("pub use std::string::greet;\n", None),
        (
            "mod detail;\n#[cfg(feature = \"x\")]\npub use detail::greet;\n",
            Some(detail),
        ),
        (
            "mod detail;\npub use detail::hello as greet;\n",
            Some("pub fn hello(name: &str) -> String {\n    name.to_owned()\n}\n"),
        ),
    ] {
        let mut new = vec![("src/lib.rs", Some(lib))];
        if let Some(detail) = detail {
            new.push(("src/detail.rs", Some(detail)));
        }
        let api = api_changes(&[("src/lib.rs", old)], &new);
        let change = api
            .modified
            .iter()
            .find(|change| change.name == "greet")
            .unwrap_or_else(|| panic!("{lib}: {api:?}"));
        assert_eq!(
            change
                .unverified
                .as_ref()
                .map(|unverified| unverified.reason),
            Some(crate::models::review::ApiModificationUnverifiedReason::ReexportTargetUnresolved),
            "{lib}"
        );
    }
}

/// 同じ差分で 2 つの項目を移して再公開しても、それぞれの再公開の先と組にする。
#[test]
fn several_reexported_moves_pair_each_target() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n\npub fn part(name: &str) -> String {\n    name.to_owned()\n}\n",
        )],
        &[
            (
                "src/lib.rs",
                Some("mod detail;\nmod other;\npub use detail::greet;\npub use other::part;\n"),
            ),
            (
                "src/detail.rs",
                Some("pub fn greet(name: &str) -> String {\n    name.to_owned()\n}\n"),
            ),
            (
                "src/other.rs",
                Some(
                    "pub fn part(name: &str, loud: bool) -> String {\n    let _ = loud;\n    name.to_owned()\n}\n",
                ),
            ),
        ],
    );
    assert!(moved(&api, "greet"), "{api:?}");
    assert_eq!(bucket(&api, "greet"), "none", "{api:?}");
    assert_eq!(bucket(&api, "part"), "mod", "{api:?}");
}

/// 再公開の先は経路で決めた宣言そのもの。同じファイルの別の inline module に足した同じ名前の
/// 宣言とは組にせず、先の既存の宣言のシグネチャと比べる。
#[test]
fn reexport_targets_are_the_resolved_declaration() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod detail;\npub fn f(_: u8) {}\n"),
            ("src/detail.rs", "pub fn f() {}\n"),
        ],
        &[
            ("src/lib.rs", Some("pub mod detail;\npub use detail::f;\n")),
            (
                "src/detail.rs",
                Some("pub fn f() {}\nmod unrelated {\n    pub fn f(_: u8) {}\n}\n"),
            ),
        ],
    );
    assert_eq!(bucket(&api, "f"), "mod", "{api:?}");
    assert!(!moved(&api, "f"), "{api:?}");
}

/// 旧いパスが残ることを確かめられない再公開 (再公開する module が条件付き、旧い宣言が inline
/// module の中にあって別のパスだった) は、移動にせず blocking に残す。
#[test]
fn reexports_that_may_lose_the_old_path_stay_blocking() {
    let detail = Some("pub fn f() {}\n");
    for (old, new) in [
        (
            vec![
                ("src/lib.rs", "pub mod api;\n"),
                ("src/api.rs", "pub fn f() {}\n"),
            ],
            vec![
                (
                    "src/lib.rs",
                    Some("#[cfg(feature = \"x\")]\npub mod api;\nmod detail;\n"),
                ),
                ("src/api.rs", Some("pub use crate::detail::f;\n")),
                ("src/detail.rs", detail),
            ],
        ),
        (
            vec![("src/lib.rs", "pub mod api {\n    pub fn f() {}\n}\n")],
            vec![
                (
                    "src/lib.rs",
                    Some("pub mod api {\n}\nmod detail;\npub use detail::f;\n"),
                ),
                ("src/detail.rs", detail),
            ],
        ),
    ] {
        let api = api_changes(&old, &new);
        assert_eq!(bucket(&api, "f"), "mod", "{new:?}: {api:?}");
        assert!(!moved(&api, "f"), "{new:?}: {api:?}");
    }
}

/// 別々だった 2 つの型を 1 つの型にまとめて再公開すると、別の型が同じ型になる (利用側の trait の
/// 実装が重なる) ので、どちらも api.mod。
#[test]
fn merging_nominal_types_behind_reexports_is_modified() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "pub struct Note;\n"),
            ("src/b.rs", "pub struct Note;\n"),
        ],
        &[
            ("src/lib.rs", Some("pub mod a;\npub mod b;\nmod detail;\n")),
            ("src/a.rs", Some("pub use crate::detail::Note;\n")),
            ("src/b.rs", Some("pub use crate::detail::Note;\n")),
            ("src/detail.rs", Some("pub struct Note;\n")),
        ],
    );
    assert_eq!(
        api.modified
            .iter()
            .filter(|change| change.name == "Note")
            .count(),
        2,
        "{api:?}"
    );
    assert!(!moved(&api, "Note"), "{api:?}");
}

/// 関数に `const` を足して移すのは、通常の移動と同じく互換 (`rust_const_fn_added`) で移動。
#[test]
fn reexported_moves_adding_const_are_compatible() {
    let api = reexported_move(
        "pub fn limit() -> u8 {\n    1\n}\n",
        "pub const fn limit() -> u8 {\n    1\n}\n",
        "limit",
    );
    assert!(
        api.compatible_modified
            .iter()
            .any(|change| change.name == "limit" && change.reason == "rust_const_fn_added"),
        "{api:?}"
    );
    assert!(moved(&api, "limit"), "{api:?}");
    assert_eq!(bucket(&api, "limit"), "mod_compat", "{api:?}");
}

/// 旧いファイル、新しいファイル (中身が None なら消す)、比べる名前の組。
type Case<'a> = (
    Vec<(&'a str, &'a str)>,
    Vec<(&'a str, Option<&'a str>)>,
    &'a str,
);

/// 旧いパスや型の同一性を確かめられない再公開は、移動にせず blocking に残す。直下と inline module
/// の中に同じ名前の宣言があった旧いファイル、非公開にした再公開の module、通常の移動と合わせて型を
/// まとめる再公開、rename したファイルの既存の型、マクロが作っていた module の型。
#[test]
fn reexports_without_identity_proof_stay_blocking() {
    let make_detail = "macro_rules! make_detail {\n    () => {\n        pub mod detail {\n            pub struct Note;\n        }\n    };\n}\n";
    let old_macro_lib = format!("{make_detail}make_detail!();\npub mod api;\n");
    let new_macro_lib =
        format!("{make_detail}pub mod detail {{\n    pub struct Note;\n}}\npub mod api;\n");
    let cases: Vec<Case<'_>> = vec![
        (
            vec![(
                "src/lib.rs",
                "pub fn f() {}\npub mod api {\n    pub fn f() {}\n}\n",
            )],
            vec![
                (
                    "src/lib.rs",
                    Some("pub mod api {}\nmod detail;\npub use detail::f;\n"),
                ),
                ("src/detail.rs", Some("pub fn f() {}\n")),
            ],
            "f",
        ),
        (
            vec![
                ("src/lib.rs", "pub mod api;\n"),
                ("src/api.rs", "pub fn f() {}\n"),
            ],
            vec![
                ("src/lib.rs", Some("mod api;\nmod detail;\n")),
                ("src/api.rs", Some("pub use crate::detail::f;\n")),
                ("src/detail.rs", Some("pub fn f() {}\n")),
            ],
            "f",
        ),
        (
            vec![
                ("src/lib.rs", "pub mod a;\npub mod b;\nmod detail;\n"),
                ("src/a.rs", "pub struct Note;\n"),
                ("src/b.rs", "pub struct Note;\n"),
                ("src/detail.rs", "pub fn helper() {}\n"),
            ],
            vec![
                (
                    "src/lib.rs",
                    Some(
                        "pub mod a;\npub mod b {\n    pub use crate::detail::Note;\n}\nmod detail;\n",
                    ),
                ),
                ("src/a.rs", Some("pub use crate::detail::Note;\n")),
                ("src/b.rs", None),
                (
                    "src/detail.rs",
                    Some("pub fn helper() {}\npub struct Note;\n"),
                ),
            ],
            "Note",
        ),
        (
            vec![
                ("src/lib.rs", "pub mod util;\npub struct Note;\n"),
                (
                    "src/util.rs",
                    "// Existing declaration.\npub struct Note;\n",
                ),
            ],
            vec![
                (
                    "src/lib.rs",
                    Some(
                        "pub mod detail;\npub use detail::Note;\npub mod util {\n    pub use crate::detail::Note;\n}\n",
                    ),
                ),
                ("src/util.rs", None),
                (
                    "src/detail.rs",
                    Some("// Existing declaration.\npub struct Note;\n"),
                ),
            ],
            "Note",
        ),
        (
            vec![
                ("src/lib.rs", old_macro_lib.as_str()),
                ("src/api.rs", "pub struct Note;\n"),
            ],
            vec![
                ("src/lib.rs", Some(new_macro_lib.as_str())),
                ("src/api.rs", Some("pub use crate::detail::Note;\n")),
            ],
            "Note",
        ),
    ];
    for (old, new, name) in &cases {
        let api = api_changes(old, new);
        assert!(
            api.modified.iter().any(|change| change.name == *name),
            "{new:?}: {api:?}"
        );
    }
}

/// 再公開を読んだファイルが crate の module の中身でない (inline にして残った古いファイル) か、
/// 先のファイルのほかの module で同じ名前の型・型別名・`pub use` を消すか変えるか足して、別の型を
/// 指していたパスが移した型を指すようになったかもしれない (マクロの展開が変わったかもしれない
/// ときも) なら、blocking に残す。
#[test]
fn stale_reexport_files_and_same_file_merges_stay_blocking() {
    let detail_after =
        |nested: &str| format!("pub struct Note;\npub mod nested {{\n    {nested}\n}}\n");
    let alias_to_moved = detail_after("pub type Note = super::Note;");
    let reexport_of_moved = detail_after("pub use super::Note;");
    let make =
        |body: &str| format!("macro_rules! make {{\n    () => {{\n        {body}\n    }};\n}}\n");
    let macro_before = format!(
        "{}pub mod nested {{\n    make!();\n}}\n",
        make("pub struct Note;")
    );
    let macro_after = format!(
        "{}pub struct Note;\npub mod nested {{\n    make!();\n}}\n",
        make("pub use crate::detail::Note;")
    );
    let cases: Vec<Case<'_>> = vec![
        (
            vec![
                ("src/lib.rs", "pub mod api;\n"),
                ("src/api.rs", "pub fn f(_: u8) {}\n"),
            ],
            vec![
                (
                    "src/lib.rs",
                    Some("pub mod api {\n    pub fn f(_: bool) {}\n}\nmod detail;\n"),
                ),
                ("src/api.rs", Some("pub use crate::detail::f;\n")),
                ("src/detail.rs", Some("pub fn f(_: u8) {}\n")),
            ],
            "f",
        ),
        (
            vec![
                ("src/lib.rs", "pub mod api;\npub mod detail;\n"),
                ("src/api.rs", "pub struct Note;\n"),
                (
                    "src/detail.rs",
                    "pub mod nested {\n    pub struct Note;\n}\n",
                ),
            ],
            vec![
                ("src/api.rs", Some("pub use crate::detail::Note;\n")),
                ("src/detail.rs", Some(reexport_of_moved.as_str())),
            ],
            "Note",
        ),
        // 型別名の指す先を移した型に変える (同じファイルの同じ名前の宣言は名前で組にできない)
        (
            vec![
                ("src/lib.rs", "pub mod api;\npub mod detail;\n"),
                ("src/api.rs", "pub struct Note;\n"),
                (
                    "src/detail.rs",
                    "pub mod nested {\n    pub type Note = u8;\n}\n",
                ),
            ],
            vec![
                ("src/api.rs", Some("pub use crate::detail::Note;\n")),
                ("src/detail.rs", Some(alias_to_moved.as_str())),
            ],
            "Note",
        ),
        // 呼び出しの字面は同じまま、同じファイルのマクロの定義を移した型の再公開に変える
        (
            vec![
                ("src/lib.rs", "pub mod api;\npub mod detail;\n"),
                ("src/api.rs", "pub struct Note;\n"),
                ("src/detail.rs", macro_before.as_str()),
            ],
            vec![
                ("src/api.rs", Some("pub use crate::detail::Note;\n")),
                ("src/detail.rs", Some(macro_after.as_str())),
            ],
            "Note",
        ),
        // glob で取り込んでいた別の型を、移した型の明示の再公開で覆う
        (
            vec![
                (
                    "src/lib.rs",
                    "pub mod api;\npub mod detail;\npub mod other;\n",
                ),
                ("src/api.rs", "pub struct Note;\n"),
                ("src/other.rs", "pub struct Note;\n"),
                (
                    "src/detail.rs",
                    "pub mod nested {\n    pub use crate::other::*;\n}\n",
                ),
            ],
            vec![
                ("src/api.rs", Some("pub use crate::detail::Note;\n")),
                (
                    "src/detail.rs",
                    Some(
                        "pub struct Note;\npub mod nested {\n    pub use crate::other::*;\n    pub use super::Note;\n}\n",
                    ),
                ),
            ],
            "Note",
        ),
        // 別の型の再公開を、移した型の再公開に変える
        (
            vec![
                (
                    "src/lib.rs",
                    "pub mod api;\npub mod detail;\npub mod other;\n",
                ),
                ("src/api.rs", "pub struct Note;\n"),
                ("src/other.rs", "pub struct Note;\n"),
                (
                    "src/detail.rs",
                    "pub mod nested {\n    pub use crate::other::Note;\n}\n",
                ),
            ],
            vec![
                ("src/api.rs", Some("pub use crate::detail::Note;\n")),
                ("src/detail.rs", Some(reexport_of_moved.as_str())),
            ],
            "Note",
        ),
    ];
    for (old, new, name) in &cases {
        let api = api_changes(old, new);
        assert!(
            api.modified.iter().any(|change| change.name == *name),
            "{new:?}: {api:?}"
        );
        // 再公開した旧い宣言は移動にしない (先のファイルの中の inline module の間の移動は出てよい)
        assert!(
            !api.moved
                .iter()
                .any(|symbol| symbol.name == *name && symbol.from != symbol.to),
            "{new:?}: {api:?}"
        );
    }
}

/// 先のファイルのほかの module にある同じ名前の型・型別名が字面のまま残るなら、指す先は変わらず
/// 型をまとめたことにならない。関数は型の名前空間を束縛しない。変えずに移した構造体は移動。
#[test]
fn kept_same_named_bindings_in_the_destination_file_do_not_block_moves() {
    for nested in [
        "pub mod nested {\n    pub type Note = u8;\n}\n",
        "pub mod nested {\n    pub struct Note;\n}\n",
        "pub mod nested {\n    #[allow(non_snake_case)]\n    pub fn Note() {}\n}\n",
    ] {
        let api = api_changes(
            &[
                ("src/lib.rs", "pub mod api;\npub mod detail;\n"),
                ("src/api.rs", "pub struct Note;\n"),
                ("src/detail.rs", nested),
            ],
            &[
                ("src/api.rs", Some("pub use crate::detail::Note;\n")),
                (
                    "src/detail.rs",
                    Some(&format!("pub struct Note;\n{nested}")),
                ),
            ],
        );
        assert!(moved(&api, "Note"), "{nested}: {api:?}");
        assert!(
            !api.modified.iter().any(|change| change.name == "Note"),
            "{nested}: {api:?}"
        );
    }
}

/// 再公開の先と同じ名前の宣言が先のファイルにほかにもあれば、名前で引く判定 (同じファイルの中だけで
/// 使う、呼び出し側の追随) で壊す変更を降格しない (Issue #282)。
#[test]
fn reexported_moves_into_files_with_same_named_items_keep_breaking_changes() {
    let api = api_changes(
        &[("src/lib.rs", "pub fn run(x: u8) {}\n")],
        &[(
            "src/lib.rs",
            Some(
                "pub mod a {\n    pub fn run(x: u8, y: u8) {}\n}\npub mod b {\n    pub fn run() {}\n    pub fn caller() {\n        run();\n    }\n}\npub use a::run;\n",
            ),
        )],
    );
    assert!(
        api.modified.iter().any(|change| change.name == "run"
            && change
                .new_signature
                .as_deref()
                .is_some_and(|signature| signature.contains("y: u8"))),
        "{api:?}"
    );
    assert!(!moved(&api, "run"), "{api:?}");
}

/// 旧いファイルの別の inline module に同じ名前の宣言があっても、直下の宣言だけを別のファイルへ
/// 移して再公開するのは移動 (どの宣言の削除かは宣言の場所で分かる。Issue #282)。
#[test]
fn reexported_moves_beside_same_named_inline_items_are_moves() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub fn run() {}\npub mod b {\n    pub fn run() {}\n}\n",
        )],
        &[
            (
                "src/lib.rs",
                Some("mod detail;\npub use detail::run;\npub mod b {\n    pub fn run() {}\n}\n"),
            ),
            ("src/detail.rs", Some("pub fn run() {}\n")),
        ],
    );
    assert!(moved(&api, "run"), "{api:?}");
    assert!(api.modified.is_empty(), "{api:?}");
}
