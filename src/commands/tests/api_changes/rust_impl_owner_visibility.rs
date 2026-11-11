//! 公開の型の inherent impl を非公開の module に置いたときの、impl の項目の公開 API 面の判定 (Issue #315)。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

const CARGO: &str =
    "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n";

/// `files` をコミットし、`changes` に書き換えた (中身が None なら消した) ときの API の差分。
fn api_changes(files: &[(&str, &str)], changes: &[(&str, Option<&str>)]) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut all = vec![("Cargo.toml", CARGO)];
    all.extend_from_slice(files);
    git_commit_files(repo, &all, "base");
    for (path, content) in changes {
        let path = repo.join(path);
        match content {
            Some(content) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).expect("mkdir");
                }
                fs::write(path, content).expect("write");
            }
            None => fs::remove_file(path).expect("remove"),
        }
    }
    detect_api_changes_from_worktree(repo)
}

/// `name` (impl の対象の型で修飾した `Local.make` も) の区分 (rm・rm_dead・add・mod・none。合わせて 2 回以上
/// 出れば dup)。
fn classify(api: &ApiChanges, name: &str) -> &'static str {
    let qualified = format!(".{name}");
    let matches = |symbol: &str| symbol == name || symbol.ends_with(&qualified);
    let count = |symbols: &[crate::models::review::ApiSymbol]| {
        symbols
            .iter()
            .filter(|symbol| matches(&symbol.name))
            .count()
    };
    let counts = [
        ("rm", count(&api.removed)),
        ("rm_dead", count(&api.removed_dead)),
        ("add", count(&api.added)),
        (
            "mod",
            api.modified
                .iter()
                .filter(|change| matches(&change.name))
                .count(),
        ),
    ];
    match counts.iter().map(|(_, count)| count).sum::<usize>() {
        0 => "none",
        1 => counts
            .iter()
            .find(|(_, count)| *count == 1)
            .map_or("none", |(label, _)| label),
        _ => "dup",
    }
}

const MAKE: &str = "impl crate::S {\n    pub fn make() -> u32 {\n        1\n    }\n}\n";

/// 報告される区分 (消す・変える・足す)。
const REPORTED: [&str; 3] = ["rm_dead", "mod", "add"];

/// 報告されない区分。
const NONE: [&str; 3] = ["none", "none", "none"];

/// `files` に、見出し `head` (`impl crate::S` など。属性も含めてよい) の inherent impl (`MAKE` の見出しを `head` に
/// 替えたもの) を `src/impls.rs` として足し、`make` を消す・`make` の戻り値の型を変える・`extra` を足したときの、
/// それぞれの区分。
fn member_changes(files: &[(&str, &str)], head: &str) -> [&'static str; 3] {
    let impls = MAKE.replace("impl crate::S", head);
    let mut all = files.to_vec();
    all.push(("src/impls.rs", &impls));
    let emptied = format!("{head} {{}}\n");
    let removed = api_changes(&all, &[("src/impls.rs", Some(&emptied))]);
    let changed = impls.replace("-> u32", "-> u64");
    let modified = api_changes(&all, &[("src/impls.rs", Some(&changed))]);
    let extended = impls.replace(
        "{\n    pub fn make",
        "{\n    pub fn extra() -> u8 {\n        0\n    }\n\n    pub fn make",
    );
    let added = api_changes(&all, &[("src/impls.rs", Some(&extended))]);
    [
        classify(&removed, "make"),
        classify(&modified, "make"),
        classify(&added, "extra"),
    ]
}

/// Issue の再現: 公開の型 `S` の inherent impl を非公開の module に置き、メソッドを消す・戻り値の型を変える・
/// メソッドを足す。どれも型 `S` を通して crate の外から見える変化。
#[test]
fn inherent_impl_members_in_private_modules_follow_the_type() {
    let changes = member_changes(
        &[("src/lib.rs", "pub struct S;\nmod impls;\n")],
        "impl crate::S",
    );
    assert_eq!(changes, REPORTED);
}

/// 対象の型の経路の書き方 (取り込みの別名、`super`、inline module の中の impl、非公開の inline module の型の
/// 再公開) によらず、impl のあるスコープで型を解き、型が外から届けば公開 API 面の中。
#[test]
fn inherent_impl_owners_are_resolved_from_the_impl_scope() {
    let lib = "pub struct S;\nmod impls;\n";
    for (lib, impls) in [
        (
            lib,
            "use crate::S as Local;\n\nimpl Local {\n    pub fn make() -> u32 {\n        1\n    }\n}\n",
        ),
        (
            lib,
            "impl super::S {\n    pub fn make() -> u32 {\n        1\n    }\n}\n",
        ),
        (
            lib,
            "mod hidden {\n    impl crate::S {\n        pub fn make() -> u32 {\n            1\n        }\n    }\n}\n",
        ),
        (
            "mod types {\n    pub struct S;\n}\npub use types::S;\nmod impls;\n",
            MAKE,
        ),
    ] {
        let api = api_changes(
            &[("src/lib.rs", lib), ("src/impls.rs", impls)],
            &[("src/impls.rs", Some(""))],
        );
        assert_eq!(classify(&api, "make"), "rm_dead", "{lib}\n{impls}\n{api:?}");
    }
}

/// 非公開の module に置いた impl の項目は、対象の型が crate の外から届かない (`pub` でない型、外から届かない
/// module の `pub` の型) と確かめられれば、今までどおり公開 API 面の外。項目を足すだけの `derive` と、impl の項目の
/// 有る全ての設定 (impl 自身や、impl のファイルを置く module の宣言の `cfg`) で有る `cfg` の付いた定義は、判定を
/// 妨げない。
#[test]
fn inherent_impl_members_of_unreachable_types_in_private_modules_are_outside() {
    for (files, head) in [
        (
            vec![(
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\npub(crate) struct S;\nmod impls;\n",
            )],
            "use std::collections::*;\n\n#[cfg(feature = \"x\")]\nimpl crate::S",
        ),
        (
            vec![(
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\nstruct S;\n#[cfg(feature = \"x\")]\nmod impls;\n",
            )],
            "use std::collections::*;\n\nimpl crate::S",
        ),
        (
            vec![(
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\nstruct S;\nmod helpers {\n    pub struct Other;\n}\nuse helpers::*;\nmod impls;\n",
            )],
            "#[cfg(feature = \"x\")]\nimpl crate::S",
        ),
        (
            vec![("src/lib.rs", "struct S;\nmod impls;\n")],
            "impl crate::S",
        ),
        (
            vec![("src/lib.rs", "pub(crate) struct S;\nmod impls;\n")],
            "impl crate::S",
        ),
        (
            vec![(
                "src/lib.rs",
                "#[derive(Debug, Clone)]\nstruct S;\nmod impls;\n",
            )],
            "impl crate::S",
        ),
        (
            vec![
                ("src/lib.rs", "mod types;\nmod impls;\n"),
                ("src/types.rs", "pub struct S;\n"),
            ],
            "impl crate::types::S",
        ),
        (
            vec![(
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\nstruct S;\nmod impls;\n",
            )],
            "#[cfg(feature = \"x\")]\nimpl crate::S",
        ),
    ] {
        let changes = member_changes(&files, head);
        assert_eq!(changes, NONE, "{files:?}");
    }
}

/// 公開の module に置いた impl の項目は、対象の型が外でも今までどおり報告する (型を外と言い切るには、
/// `#[path]`・`include!` による impl のファイルの別の置き場なども確かめる必要がある)。
#[test]
fn inherent_impl_members_in_public_modules_are_reported_as_before() {
    for files in [
        vec![("src/lib.rs", "struct S;\npub mod impls;\n")],
        vec![("src/lib.rs", "pub(crate) struct S;\npub mod impls;\n")],
        // 同じファイルを `#[path]` で、公開の型 `a::S` のある module にも置く (`super::S` が別の型を指す)
        vec![
            ("src/lib.rs", "struct S;\npub mod impls;\npub mod a;\n"),
            (
                "src/a.rs",
                "pub struct S;\n#[path = \"impls.rs\"]\nmod details;\n",
            ),
        ],
        // 同じファイルを `include!` で取り込む
        vec![
            ("src/lib.rs", "struct S;\npub mod impls;\npub mod a;\n"),
            (
                "src/a.rs",
                "pub struct S;\nmod details {\n    include!(\"impls.rs\");\n}\n",
            ),
        ],
    ] {
        let head = if files.len() > 1 {
            "impl super::S"
        } else {
            "impl crate::S"
        };
        let changes = member_changes(&files, head);
        assert_eq!(changes, REPORTED, "{files:?}");
    }
}

/// 対象の型が外から届き得る (公開の関数の signature、公開の関連定数の型、`Self` を返す impl の対象の型の引数に
/// 現れる型) か、決め切れない (型別名、型そのものを置き換え得る属性マクロの付いた定義、impl に付いた属性マクロ、
/// impl の項目の有る設定のどこかで無い定義や取り込み (経路の module の区切りの別名、glob の先の取り込み、条件付きの
/// ファイルを含む。無い設定では glob などの別の束縛に替わり得る)、非公開の inline module の中の型。型そのものの
/// 公開 API 面も決めない) なら、非公開の module に置いた impl の項目も公開として扱う。
#[test]
fn inherent_impl_members_of_undecided_types_are_reported() {
    let selected = "pub mod original {\n    pub struct S;\n}\n\nmod hidden {\n    pub(crate) struct Hidden;\n}\n\nmod selected {\n    #[cfg(feature = \"x\")]\n    pub(crate) use crate::hidden::Hidden as S;\n    pub use crate::original::*;\n}\n\nmod impls;\n";
    for (files, head) in [
        (
            vec![(
                "src/lib.rs",
                "pub mod original {\n    pub struct S;\n}\nmod hidden {\n    pub(crate) struct S;\n}\nmod alternatives {\n    pub use crate::original as chosen;\n}\nmod impls;\n",
            )],
            "#[cfg(feature = \"x\")]\nuse crate::hidden as chosen;\nuse crate::alternatives::*;\n\nimpl chosen::S",
        ),
        (
            vec![("src/lib.rs", selected)],
            "use crate::selected::*;\n\nimpl S",
        ),
        (
            vec![
                (
                    "src/lib.rs",
                    "pub mod original {\n    pub struct S;\n}\nmod alternatives {\n    pub use crate::original as chosen;\n}\nuse alternatives::*;\nmod chosen;\nmod impls;\n",
                ),
                (
                    "src/chosen.rs",
                    "#![cfg(feature = \"y\")]\n\n#[cfg(feature = \"x\")]\npub(crate) struct S;\n",
                ),
            ],
            "#[cfg(feature = \"x\")]\nimpl crate::chosen::S",
        ),
        (vec![("src/lib.rs", selected)], "impl crate::selected::S"),
        (
            vec![(
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\nstruct S;\nmod helpers {\n    pub struct Other;\n}\nuse helpers::*;\nmod impls;\n",
            )],
            "impl crate::S",
        ),
        (
            vec![(
                "src/lib.rs",
                "#[derive(Debug)]\n#[expose]\nstruct S;\nmod impls;\n",
            )],
            "impl crate::S",
        ),
        (
            vec![
                (
                    "src/lib.rs",
                    "mod types;\npub fn get() -> types::S {\n    types::S\n}\nmod impls;\n",
                ),
                ("src/types.rs", "pub struct S;\n"),
            ],
            "impl crate::types::S",
        ),
        (
            vec![
                (
                    "src/lib.rs",
                    "mod types;\nmod impls;\npub struct Public;\n\nimpl Public {\n    pub const VALUE: types::S = types::S;\n}\n",
                ),
                ("src/types.rs", "pub struct S;\n"),
            ],
            "impl crate::types::S",
        ),
        (
            vec![
                (
                    "src/lib.rs",
                    "mod types;\nmod impls;\npub struct Public<T>(pub T);\n\nimpl Public<types::S> {\n    pub fn new() -> Self {\n        Public(types::S)\n    }\n}\n",
                ),
                ("src/types.rs", "pub struct S;\n"),
            ],
            "impl crate::types::S",
        ),
        (
            vec![(
                "src/lib.rs",
                "pub struct S;\npub type Alias = S;\nmod impls;\n",
            )],
            "impl crate::Alias",
        ),
        (
            vec![(
                "src/lib.rs",
                "pub(crate) struct Hidden;\npub struct S;\nmod impls;\n",
            )],
            "#[expose]\nimpl crate::Hidden",
        ),
        (
            vec![
                (
                    "src/lib.rs",
                    "pub mod original;\npub mod types;\nmod impls;\n",
                ),
                ("src/original.rs", "pub struct S;\n"),
                (
                    "src/types.rs",
                    "#[cfg(feature = \"x\")]\npub(crate) struct S;\npub use crate::original::*;\n",
                ),
            ],
            "impl crate::types::S",
        ),
        (
            vec![(
                "src/lib.rs",
                "mod types {\n    pub struct S;\n}\nuse types::S;\nmod impls;\n",
            )],
            "impl crate::S",
        ),
    ] {
        let changes = member_changes(&files, head);
        assert_eq!(changes, REPORTED, "{files:?}");
    }
}

/// impl を置いた module の宣言を変えても、impl の項目は対象の型に従う。公開の型なら module を非公開にしても
/// 消えず、宣言を消す・足すと消える・現れる。外から届かない型なら、非公開の module の項目は外のままで、公開の
/// module の項目は今までどおり置き場に従う。
#[test]
fn inherent_impl_members_follow_the_type_when_their_module_changes() {
    for (struct_item, old, new, expected) in [
        (
            "pub struct S;\n",
            "pub mod impls;\n",
            "mod impls;\n",
            "none",
        ),
        ("pub struct S;\n", "mod impls;\n", "", "rm_dead"),
        ("pub struct S;\n", "", "mod impls;\n", "add"),
        ("struct S;\n", "mod impls;\n", "", "none"),
        ("struct S;\n", "", "mod impls;\n", "none"),
        ("struct S;\n", "pub mod impls;\n", "mod impls;\n", "rm_dead"),
        ("struct S;\n", "pub mod impls;\n", "", "rm_dead"),
    ] {
        let old_lib = format!("{struct_item}{old}");
        let new_lib = format!("{struct_item}{new}");
        let api = api_changes(
            &[("src/lib.rs", &old_lib), ("src/impls.rs", MAKE)],
            &[("src/lib.rs", Some(&new_lib))],
        );
        assert_eq!(
            classify(&api, "make"),
            expected,
            "{old_lib}\n{new_lib}\n{api:?}"
        );
    }
}

/// impl を置いたファイルを module の宣言ごと消す・足すときも、impl の項目は対象の型に従う。
#[test]
fn inherent_impl_members_follow_the_type_when_their_file_is_added_or_removed() {
    for (struct_item, declaration, expected_removal, expected_addition) in [
        ("pub struct S;\n", "mod impls;\n", "rm_dead", "add"),
        ("struct S;\n", "mod impls;\n", "none", "none"),
        ("struct S;\n", "pub mod impls;\n", "rm_dead", "add"),
    ] {
        let with = format!("{struct_item}{declaration}");
        let api = api_changes(
            &[("src/lib.rs", &with), ("src/impls.rs", MAKE)],
            &[("src/lib.rs", Some(struct_item)), ("src/impls.rs", None)],
        );
        assert_eq!(classify(&api, "make"), expected_removal, "{with}\n{api:?}");
        let api = api_changes(
            &[("src/lib.rs", struct_item)],
            &[("src/lib.rs", Some(&with)), ("src/impls.rs", Some(MAKE))],
        );
        assert_eq!(classify(&api, "make"), expected_addition, "{with}\n{api:?}");
    }
}

/// module の宣言を消し、同じ差分で impl のファイルからも項目を消す・足すときも、ファイルごとの比較と module の
/// 比べ直しの両方が出さない (どちらか一方が 1 回だけ出す)。
#[test]
fn inherent_impl_members_removed_with_their_module_are_reported_once() {
    let lib = "pub struct S;\nmod impls;\n";
    let api = api_changes(
        &[("src/lib.rs", lib), ("src/impls.rs", MAKE)],
        &[
            ("src/lib.rs", Some("pub struct S;\n")),
            ("src/impls.rs", Some("impl crate::S {}\n")),
        ],
    );
    assert_eq!(classify(&api, "make"), "rm_dead", "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub struct S;\n"),
            ("src/impls.rs", "impl crate::S {}\n"),
        ],
        &[("src/lib.rs", Some(lib)), ("src/impls.rs", Some(MAKE))],
    );
    assert_eq!(classify(&api, "make"), "add", "{api:?}");
}
