//! Rust の公開シンボルの契約が名指す別名の指す先を変えた差分 (Issue #283)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

use crate::models::review::ApiModificationUnverifiedReason as Reason;

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

/// blocking な api.mod のうち、名前が `name` のものの未検証の理由。
fn dependency_reason(api: &crate::models::review::ApiChanges, name: &str) -> Option<Reason> {
    api.modified
        .iter()
        .find(|change| change.name == name)
        .and_then(|change| change.unverified.as_ref())
        .map(|unverified| unverified.reason)
}

/// Issue #283 の再現: `use … as` の別名と非公開の型別名の指す先を変えると、シグネチャの字面が
/// 同じでも blocking な api.mod。
#[test]
fn alias_target_changes_are_modified() {
    for (old, new) in [
        (
            "use std::path::PathBuf as Text;\n",
            "use std::string::String as Text;\n",
        ),
        ("type Text = std::path::PathBuf;\n", "type Text = String;\n"),
        // 別名の連鎖の奥だけを変える
        (
            "type Inner = std::path::PathBuf;\ntype Text = Inner;\n",
            "type Inner = String;\ntype Text = Inner;\n",
        ),
    ] {
        let function = "\npub fn label(t: Text) -> usize {\n    0\n}\n";
        let api = api_changes(&format!("{old}{function}"), &format!("{new}{function}"));
        assert_eq!(
            dependency_reason(&api, "label"),
            Some(Reason::RustSignatureDependencyChanged),
            "{new}: {api:?}"
        );
    }

    // 公開構造体のフィールドと定数の型
    let api = api_changes(
        "type Text = u8;\npub struct Note {\n    pub text: Text,\n}\npub const LIMIT: Text = 1;\n",
        "type Text = u16;\npub struct Note {\n    pub text: Text,\n}\npub const LIMIT: Text = 1;\n",
    );
    assert_eq!(
        dependency_reason(&api, "Note"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
    assert_eq!(
        dependency_reason(&api, "LIMIT"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// 指す先が変わらない変更 (本体・引数名・無関係な use・ジェネリクスの同じ名前) は出さない。
#[test]
fn unchanged_dependencies_are_not_reported() {
    let api = api_changes(
        "use std::path::PathBuf as Text;\nuse std::fmt;\n\npub fn label(t: Text) -> usize {\n    0\n}\n\npub fn generic<Text>(_: Text) {}\n",
        "use std::path::PathBuf as Text;\n\npub fn label(t: Text) -> usize {\n    1\n}\n\npub fn generic<Text>(_: Text) {}\n",
    );
    assert!(api.modified.is_empty(), "{api:?}");

    let api = api_changes(
        "use std::path::PathBuf as Text;\npub fn generic<Text>(_: Text) {}\n",
        "use std::string::String as Text;\npub fn generic<Text>(_: Text) {}\n",
    );
    assert!(api.modified.is_empty(), "{api:?}");
}

/// 引数名・const の追加・互換なフィールドの追加と同時に別名の指す先を変えても、互換や変更なしに
/// せず blocking。
#[test]
fn dependency_changes_override_compatible_classifications() {
    let api = api_changes(
        "type Text = std::path::PathBuf;\npub fn label(value: Text) {}\n",
        "type Text = String;\npub fn label(text: Text) {}\n",
    );
    assert_eq!(
        dependency_reason(&api, "label"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );

    let api = api_changes(
        "type Text = u8;\npub fn limit(value: Text) -> u8 {\n    0\n}\n",
        "type Text = u16;\npub const fn limit(value: Text) -> u8 {\n    0\n}\n",
    );
    assert_eq!(
        dependency_reason(&api, "limit"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
    assert!(
        api.compatible_modified
            .iter()
            .all(|change| change.name != "limit"),
        "{api:?}"
    );

    let api = api_changes(
        "type Text = u8;\npub struct Note {\n    pub text: Text,\n    hidden: u8,\n}\n",
        "type Text = u16;\npub struct Note {\n    pub text: Text,\n    pub extra: u8,\n    hidden: u8,\n}\n",
    );
    assert_eq!(
        dependency_reason(&api, "Note"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// 透過的な公開の型別名の導入は、片側にしか無い名前なので今までどおり互換。
#[test]
fn transparent_public_alias_introduction_stays_compatible() {
    let api = api_changes(
        "pub fn label(_: std::string::String) {}\n",
        "pub type Text = std::string::String;\npub fn label(_: Text) {}\n",
    );
    assert!(
        api.modified.iter().all(|change| change.name != "label"),
        "{api:?}"
    );
}

/// 公開の型別名の右辺を変えると、別名自身とそれを名指すシグネチャの両方を出す。同じ場所の cfg の
/// 分岐も、分岐ごとに名指す名前の束縛を比べる。
#[test]
fn public_aliases_and_cfg_variants_are_followed() {
    let api = api_changes(
        "pub type Text = u8;\npub fn label(t: Text) {}\n",
        "pub type Text = u16;\npub fn label(t: Text) {}\n",
    );
    assert!(
        api.modified.iter().any(|change| change.name == "Text"),
        "{api:?}"
    );
    assert_eq!(
        dependency_reason(&api, "label"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );

    let api = api_changes(
        "#[cfg(unix)]\nmod sys {\n    pub type Text = u8;\n}\n#[cfg(windows)]\nmod sys {\n    pub type Text = u8;\n}\n#[cfg(unix)]\npub fn label(t: sys::Text) {}\n#[cfg(windows)]\npub fn label(t: sys::Text) {}\n",
        "#[cfg(unix)]\nmod sys {\n    pub type Text = u16;\n}\n#[cfg(windows)]\nmod sys {\n    pub type Text = u8;\n}\n#[cfg(unix)]\npub fn label(t: sys::Text) {}\n#[cfg(windows)]\npub fn label(t: sys::Text) {}\n",
    );
    assert!(dependency_reason(&api, "label").is_some(), "{api:?}");
}

/// ファイルの組を `old` でコミットし、`new` に書き換えたときの API の差分。
fn api_changes_of_files(
    old: &[(&str, &str)],
    new: &[(&str, &str)],
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut files = vec![("Cargo.toml", CARGO)];
    files.extend_from_slice(old);
    git_commit_files(repo, &files, "base");
    for (path, content) in new {
        fs::write(repo.join(path), content).expect("write");
    }
    detect_api_changes_from_worktree(repo)
}

/// 型を別のファイルへ移して取り込むか、同じファイルの別の module へ移すのは、名指す先の型が同じ
/// なので変化にしない (型の移動そのものは型の変更として出る)。移すときに型別名の右辺を変えるか、
/// 元からあった別の型に差し替えるのは変化。
#[test]
fn moved_definitions_are_not_dependency_changes() {
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod consumer;\npub mod shapes;\n"),
            (
                "src/consumer.rs",
                "pub struct Shape {\n    pub width: u8,\n}\n\npub fn make() -> Shape {\n    Shape { width: 1 }\n}\n",
            ),
            ("src/shapes.rs", "pub fn other() {}\n"),
        ],
        &[
            (
                "src/consumer.rs",
                "use crate::shapes::Shape;\n\npub fn make() -> Shape {\n    Shape { width: 1 }\n}\n",
            ),
            (
                "src/shapes.rs",
                "pub fn other() {}\n\npub struct Shape {\n    pub width: u8,\n}\n",
            ),
        ],
    );
    assert!(
        api.modified.iter().all(|change| change.name != "make"),
        "{api:?}"
    );

    let api = api_changes(
        "pub mod a {\n    pub struct Shape;\n}\npub mod b {}\npub use a::Shape;\npub fn make(_: Shape) {}\n",
        "pub mod a {}\npub mod b {\n    pub struct Shape;\n}\npub use b::Shape;\npub fn make(_: Shape) {}\n",
    );
    assert!(
        api.modified.iter().all(|change| change.name != "make"),
        "{api:?}"
    );
}

/// 型別名を別のファイルへ移すときに右辺を変えるか、元からあった別のファイルの型に差し替えるのは、
/// 名指す先の変化。
#[test]
fn moved_alias_changes_and_replacements_are_changes() {
    let base = [
        (
            "src/lib.rs",
            "pub mod consumer;\npub mod shapes;\npub mod time;\n",
        ),
        (
            "src/consumer.rs",
            "type Text = u8;\n\npub fn label(_: Text) {}\n",
        ),
        ("src/shapes.rs", "pub fn other() {}\n"),
        ("src/time.rs", "pub struct Duration;\n"),
    ];
    let consumer = (
        "src/consumer.rs",
        "use crate::shapes::Text;\n\npub fn label(_: Text) {}\n",
    );
    // 右辺を変えずに移すのは変化でない
    let api = api_changes_of_files(
        &base,
        &[
            consumer,
            (
                "src/shapes.rs",
                "pub fn other() {}\n\npub(crate) type Text = u8;\n",
            ),
        ],
    );
    assert_eq!(dependency_reason(&api, "label"), None, "{api:?}");
    // 移すときに右辺を変える
    let api = api_changes_of_files(
        &base,
        &[
            consumer,
            (
                "src/shapes.rs",
                "pub fn other() {}\n\npub(crate) type Text = u16;\n",
            ),
        ],
    );
    assert_eq!(
        dependency_reason(&api, "label"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
    // 型別名を、差分の外にある別のファイルの型に差し替える
    let api = api_changes_of_files(
        &base,
        &[(
            "src/consumer.rs",
            "use crate::time::Duration as Text;\n\npub fn label(_: Text) {}\n",
        )],
    );
    assert_eq!(
        dependency_reason(&api, "label"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}
