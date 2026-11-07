//! Rust の同じファイルの別々の inline module に同じ名前の公開項目がある差分 (Issue #282)。

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

/// blocking な api.mod のうち、名前が `name` で新しい signature に `marker` を含むものの数。
fn modified(api: &crate::models::review::ApiChanges, name: &str, marker: &str) -> usize {
    api.modified
        .iter()
        .filter(|change| {
            change.name == name
                && change
                    .new_signature
                    .as_deref()
                    .is_some_and(|signature| signature.contains(marker))
        })
        .count()
}

/// Issue #282 の再現: 片方の inline module の同じ名前の関数・構造体・メソッドを変えると api.mod。
#[test]
fn changing_one_of_same_named_items_is_modified() {
    let api = api_changes(
        "pub mod a {\n    pub fn run(x: u8) -> u8 {\n        x\n    }\n}\npub mod b {\n    pub fn run() {}\n}\n",
        "pub mod a {\n    pub fn run(x: u8, y: u8) -> u8 {\n        x + y\n    }\n}\npub mod b {\n    pub fn run() {}\n}\n",
    );
    assert_eq!(modified(&api, "run", "y: u8"), 1, "{api:?}");
    assert_eq!(api.modified.len(), 1, "{api:?}");
    // 出力には、同じ名前の項目を区別する inline module の経路が付く
    assert_eq!(
        api.modified[0].inline_module.as_deref(),
        Some("a"),
        "{api:?}"
    );
    let json = serde_json::to_string(&api).expect("json");
    assert!(json.contains("\"inline_module\":\"a\""), "{json}");

    let api = api_changes(
        "pub mod a {\n    pub struct Note {\n        pub text: u8,\n    }\n}\npub mod b {\n    pub struct Note {\n        pub tag: u8,\n    }\n}\n",
        "pub mod a {\n    pub struct Note {\n        pub body: u8,\n    }\n}\npub mod b {\n    pub struct Note {\n        pub tag: u8,\n    }\n}\n",
    );
    assert_eq!(modified(&api, "Note", "body"), 1, "{api:?}");

    let api = api_changes(
        "pub mod a {\n    pub struct Foo;\n    impl Foo {\n        pub fn run(&self, x: u8) {}\n    }\n}\npub mod b {\n    pub struct Foo;\n    impl Foo {\n        pub fn run(&self) {}\n    }\n}\n",
        "pub mod a {\n    pub struct Foo;\n    impl Foo {\n        pub fn run(&self, x: u8, y: u8) {}\n    }\n}\npub mod b {\n    pub struct Foo;\n    impl Foo {\n        pub fn run(&self) {}\n    }\n}\n",
    );
    assert_eq!(modified(&api, "Foo.run", "y: u8"), 1, "{api:?}");
    assert_eq!(
        api.modified[0].inline_module.as_deref(),
        Some("a"),
        "{api:?}"
    );
}

/// 両方を変えれば 2 件、signature を入れ替えても両方の変更、同じ signature の片方を消せば削除。
#[test]
fn same_named_items_are_compared_per_declaration() {
    let both = api_changes(
        "pub mod a {\n    pub fn run(x: u8) {}\n}\npub mod b {\n    pub fn run() {}\n}\n",
        "pub mod a {\n    pub fn run(x: u16) {}\n}\npub mod b {\n    pub fn run(y: bool) {}\n}\n",
    );
    assert_eq!(modified(&both, "run", "u16"), 1, "{both:?}");
    assert_eq!(modified(&both, "run", "bool"), 1, "{both:?}");

    let swapped = api_changes(
        "pub mod a {\n    pub fn run(x: u8) {}\n}\npub mod b {\n    pub fn run() {}\n}\n",
        "pub mod a {\n    pub fn run() {}\n}\npub mod b {\n    pub fn run(x: u8) {}\n}\n",
    );
    assert_eq!(swapped.modified.len(), 2, "{swapped:?}");

    let removed = api_changes(
        "pub mod a {\n    pub fn run() {}\n}\npub mod b {\n    pub fn run() {}\n}\n",
        "pub mod a {}\npub mod b {\n    pub fn run() {}\n}\n",
    );
    assert!(
        removed
            .removed
            .iter()
            .chain(&removed.removed_dead)
            .any(|symbol| symbol.name == "run" && symbol.inline_module.as_deref() == Some("a")),
        "{removed:?}"
    );

    // 並べ替えと本体だけの変更は何も出さない
    let reordered = api_changes(
        "pub mod a {\n    pub fn run() {}\n}\npub mod b {\n    pub fn run(x: u8) {}\n}\n",
        "pub mod b {\n    pub fn run(x: u8) {\n        let _ = x;\n    }\n}\npub mod a {\n    pub fn run() {}\n}\n",
    );
    assert!(reordered.modified.is_empty(), "{reordered:?}");
    assert!(reordered.removed.is_empty(), "{reordered:?}");
    assert!(reordered.added.is_empty(), "{reordered:?}");
}

/// 同じ名前の別の宣言への呼び出しや、別の宣言の型別名の証明で、変えた宣言を降格しない。
#[test]
fn proofs_by_name_do_not_cover_other_same_named_items() {
    // b::run だけが同じファイルの中で呼ばれていても、a::run は同じファイルの中だけで使う
    // 関数ではない
    let api = api_changes(
        "pub mod a {\n    pub fn run(x: u8) {}\n}\npub mod b {\n    pub fn run() {}\n    pub fn caller() {\n        run();\n    }\n}\n",
        "pub mod a {\n    pub fn run(x: u8, y: u8) {}\n}\npub mod b {\n    pub fn run() {}\n    pub fn caller() {\n        run();\n    }\n}\n",
    );
    assert_eq!(modified(&api, "run", "y: u8"), 1, "{api:?}");

    // 直下の run の型別名の導入が同値でも、inline module の run の破壊的な変更は降格しない
    let api = api_changes(
        "pub fn run(x: u8) {}\npub mod a {\n    pub fn run(x: u8) {}\n}\n",
        "pub type Num = u8;\npub fn run(x: Num) {}\npub mod a {\n    pub fn run(x: u8, y: u8) {}\n}\n",
    );
    assert_eq!(modified(&api, "run", "y: u8"), 1, "{api:?}");
}

/// 直下の構造体と inline module の同じ名前の型別名が並んでも、型別名の変更を比べる。構造体の
/// 形は組にした宣言そのものから読むので、互換な変更は互換のまま。
#[test]
fn same_named_items_of_other_kinds_and_shapes_are_compared() {
    let api = api_changes(
        "pub struct Note;\npub mod nested {\n    pub type Note = u8;\n}\n",
        "pub struct Note;\npub mod nested {\n    pub type Note = u16;\n}\n",
    );
    assert_eq!(modified(&api, "Note", "u16"), 1, "{api:?}");

    let api = api_changes(
        "pub mod a {\n    pub struct Note {\n        pub text: u8,\n        hidden: u8,\n    }\n}\npub mod b {\n    pub struct Note {\n        pub tag: u8,\n    }\n}\n",
        "pub mod a {\n    pub struct Note {\n        pub text: u8,\n        pub extra: u8,\n        hidden: u8,\n    }\n}\npub mod b {\n    pub struct Note {\n        pub tag: u8,\n    }\n}\n",
    );
    assert!(api.modified.is_empty(), "{api:?}");
    assert!(
        api.compatible_modified
            .iter()
            .any(|change| change.name == "Note"),
        "{api:?}"
    );
}

/// 同じ場所の同じ名前の宣言 (cfg の分岐) は組にできない。条件の間で signature を入れ替えると、
/// signature の集まりが同じでも blocking。同じ signature の分岐を 1 つにまとめるのは何も出さない。
#[test]
fn same_place_declarations_compare_conditions_with_signatures() {
    let api = api_changes(
        "#[cfg(unix)]\npub fn run(x: u8) {}\n#[cfg(windows)]\npub fn run(x: u16) {}\n",
        "#[cfg(unix)]\npub fn run(x: u16) {}\n#[cfg(windows)]\npub fn run(x: u8) {}\n",
    );
    let change = api
        .modified
        .iter()
        .find(|change| change.name == "run")
        .unwrap_or_else(|| panic!("{api:?}"));
    assert_eq!(
        change
            .unverified
            .as_ref()
            .map(|unverified| unverified.reason),
        Some(
            crate::models::review::ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous
        ),
        "{api:?}"
    );

    let api = api_changes(
        "#[cfg(unix)]\npub fn run(x: u8) {}\n#[cfg(windows)]\npub fn run(x: u8) {}\n",
        "pub fn run(x: u8) {}\n",
    );
    assert!(api.modified.is_empty(), "{api:?}");
    assert!(
        api.removed.is_empty() && api.removed_dead.is_empty(),
        "{api:?}"
    );
}

/// `src/lib.rs` とほかのファイルを `old` でコミットし、`new` に書き換えた (中身が None なら消した)
/// ときの API の差分。
fn api_changes_of_files(
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

/// 同じ module の別々の型の impl (`impl a::Foo` と `impl b::Foo`) のメソッドは、impl の対象の型で
/// 区別する。片方の impl を消せば削除、signature を入れ替えれば両方の変更。
#[test]
fn methods_of_impls_for_different_types_are_distinguished() {
    let types = "pub mod a {\n    pub struct Foo;\n}\npub mod b {\n    pub struct Foo;\n}\n";
    let impls = |a: &str, b: &str| {
        let mut source = types.to_owned();
        source.push_str(&format!(
            "impl a::Foo {{\n    pub fn run(&self{a}) {{}}\n}}\n"
        ));
        source.push_str(&format!(
            "impl b::Foo {{\n    pub fn run(&self{b}) {{}}\n}}\n"
        ));
        source
    };
    let api = api_changes(&impls(", x: u8", ""), &impls(", x: u8, y: u8", ""));
    assert_eq!(api.modified.len(), 1, "{api:?}");
    assert!(
        api.modified[0]
            .new_signature
            .as_deref()
            .is_some_and(|signature| signature.contains("y: u8")),
        "{api:?}"
    );

    let api = api_changes(&impls(", x: u8", ", x: u16"), &impls(", x: u16", ", x: u8"));
    assert_eq!(api.modified.len(), 2, "{api:?}");

    let api = api_changes(&impls("", ""), &impls_without_a(types));
    assert!(
        api.removed
            .iter()
            .chain(&api.removed_dead)
            .any(|symbol| symbol.name.ends_with("run")),
        "{api:?}"
    );
}

/// `impl a::Foo` を除いた、`impl b::Foo` だけのソース。
fn impls_without_a(types: &str) -> String {
    format!("{types}impl b::Foo {{\n    pub fn run(&self) {{}}\n}}\n")
}

/// 1 つの宣言を同じファイルの 2 つの inline module に分けると、どちらへの移動か決められないので、
/// 移動にせず削除に残す。
#[test]
fn moves_to_several_inline_modules_are_not_paired() {
    let api = api_changes(
        "pub mod a {\n    pub fn run() {}\n}\n",
        "pub mod a {}\npub mod b {\n    pub fn run() {}\n}\npub mod c {\n    pub fn run() {}\n}\n",
    );
    assert!(api.moved.is_empty(), "{api:?}");
    assert!(
        api.removed
            .iter()
            .chain(&api.removed_dead)
            .any(|symbol| symbol.name == "run" && symbol.inline_module.as_deref() == Some("a")),
        "{api:?}"
    );

    // 1 つの inline module への移動は、移動元と移動先の経路を添えた移動
    let api = api_changes(
        "pub mod a {\n    pub fn run() {}\n}\n",
        "pub mod a {}\npub mod b {\n    pub fn run() {}\n}\n",
    );
    assert!(
        api.moved.iter().any(|symbol| symbol.name == "run"
            && symbol.from_inline_module.as_deref() == Some("a")
            && symbol.to_inline_module.as_deref() == Some("b")),
        "{api:?}"
    );
}

/// 非公開の同じ名前の関数や、別の型の同じ名前のメソッドへの呼び出しで、変えた宣言を同じファイルの
/// 中だけで使うものとみなさない。
#[test]
fn private_or_other_owner_calls_do_not_close_changes() {
    let api = api_changes(
        "pub mod a {\n    pub fn run(x: u8) {}\n}\nmod b {\n    fn run() {}\n    pub fn caller() {\n        run();\n    }\n}\n",
        "pub mod a {\n    pub fn run(x: u8, y: u8) {}\n}\nmod b {\n    fn run() {}\n    pub fn caller() {\n        run();\n    }\n}\n",
    );
    assert_eq!(modified(&api, "run", "y: u8"), 1, "{api:?}");

    let api = api_changes(
        "pub struct Foo;\nimpl Foo {\n    pub fn run(&self, x: u8) {}\n}\npub struct Bar;\nimpl Bar {\n    pub fn run(&self) {}\n    pub fn caller(&self) {\n        self.run();\n    }\n}\n",
        "pub struct Foo;\nimpl Foo {\n    pub fn run(&self, x: u8, y: u8) {}\n}\npub struct Bar;\nimpl Bar {\n    pub fn run(&self) {}\n    pub fn caller(&self) {\n        self.run();\n    }\n}\n",
    );
    assert_eq!(modified(&api, "Foo.run", "y: u8"), 1, "{api:?}");
}

/// 新規ファイルと削除ファイルの項目にも、inline module の経路を添える。
#[test]
fn added_and_deleted_files_carry_inline_modules() {
    let items = "pub mod a {\n    pub fn run() {}\n}\npub mod b {\n    pub fn run() {}\n}\n";
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod old;\npub fn keep() {}\n"),
            ("src/old.rs", items),
        ],
        &[
            ("src/lib.rs", Some("pub fn keep() {}\n")),
            ("src/old.rs", None),
        ],
    );
    let modules = |symbols: &[crate::models::review::ApiSymbol]| {
        let mut modules: Vec<Option<String>> = symbols
            .iter()
            .filter(|symbol| symbol.name == "run")
            .map(|symbol| symbol.inline_module.clone())
            .collect();
        modules.sort();
        modules
    };
    let removed: Vec<_> = api
        .removed
        .iter()
        .chain(&api.removed_dead)
        .cloned()
        .collect();
    assert_eq!(
        modules(&removed),
        [Some("a".to_owned()), Some("b".to_owned())],
        "{api:?}"
    );

    let api = api_changes_of_files(
        &[("src/lib.rs", "pub fn keep() {}\n")],
        &[
            ("src/lib.rs", Some("pub fn keep() {}\npub mod extra;\n")),
            ("src/extra.rs", Some(items)),
        ],
    );
    assert_eq!(
        modules(&api.added),
        [Some("a".to_owned()), Some("b".to_owned())],
        "{api:?}"
    );
}
