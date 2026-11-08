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

/// ファイルの組 `old` をコミットし、`edit` で作業ツリーを変えたときの API の差分。
fn api_changes_after(
    old: &[(&str, &str)],
    edit: impl FnOnce(&std::path::Path),
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut files = vec![("Cargo.toml", CARGO)];
    files.extend_from_slice(old);
    git_commit_files(repo, &files, "base");
    edit(repo);
    detect_api_changes_from_worktree(repo)
}

/// Issue #300 の再現の、変更前のファイル。
const ISSUE_300_BASE: [(&str, &str); 3] = [
    ("src/lib.rs", "pub mod api;\nmod error;\n"),
    (
        "src/error.rs",
        "pub(crate) type Result<T> = std::result::Result<T, String>;\n",
    ),
    (
        "src/api.rs",
        "use crate::error::Result;\n\npub fn run() -> Result<()> {\n    Ok(())\n}\n",
    ),
];

/// Issue #300 の再現: ほかのファイルの非公開の型別名の右辺を変えると、それを名指す公開関数は、名指す側の
/// ファイルを同じ差分で変えても変えなくても blocking な api.mod。右辺を変えない変更や、無関係な束縛の
/// 変更では出さない。
#[test]
fn other_file_alias_changes_are_followed() {
    let error = (
        "src/error.rs",
        "pub(crate) type Result<T> = std::result::Result<T, std::io::Error>;\n",
    );
    // 名指す側のファイルは本体だけ変える
    let api = api_changes_of_files(
        &ISSUE_300_BASE,
        &[
            error,
            (
                "src/api.rs",
                "use crate::error::Result;\n\npub fn run() -> Result<()> {\n    // ok\n    Ok(())\n}\n",
            ),
        ],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
    // 名指す側のファイルを変えない
    let api = api_changes_of_files(&ISSUE_300_BASE, &[error]);
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
    // 右辺を変えない・無関係な束縛だけを変える
    for unrelated in [
        "pub(crate) type Result<T> = std::result::Result<T, String>;\n\nfn helper() {}\n",
        "pub(crate) type Result<T> = std::result::Result<T, String>;\n\npub(crate) type Other = u8;\n",
    ] {
        let api = api_changes_of_files(&ISSUE_300_BASE, &[("src/error.rs", unrelated)]);
        assert_eq!(dependency_reason(&api, "run"), None, "{unrelated}: {api:?}");
    }
}

/// 変えていないファイルの再公開・型別名の右辺・`super::`・module の別名と glob を経て、変えたファイルの
/// 束縛に届くときも比べる。外へ出た鎖が変えたファイルへ戻るときも。
#[test]
fn bindings_reached_through_unchanged_files_are_followed() {
    let changed = |api: &crate::models::review::ApiChanges, name: &str| {
        assert_eq!(
            dependency_reason(api, name),
            Some(Reason::RustSignatureDependencyChanged),
            "{name}: {api:?}"
        );
    };
    // 再公開を経る
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod api;\nmod error;\nmod prelude;\n"),
            (
                "src/error.rs",
                "pub(crate) type Result<T> = std::result::Result<T, String>;\n",
            ),
            ("src/prelude.rs", "pub(crate) use crate::error::Result;\n"),
            (
                "src/api.rs",
                "use crate::prelude::Result;\n\npub fn run() -> Result<()> {\n    Ok(())\n}\n",
            ),
        ],
        &[(
            "src/error.rs",
            "pub(crate) type Result<T> = std::result::Result<T, u8>;\n",
        )],
    );
    changed(&api, "run");
    // 型別名の右辺を経る (名指す側は変えた名前を書いていない)
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod api;\nmod bridge;\nmod raw;\n"),
            ("src/raw.rs", "pub(crate) type Raw = u32;\n"),
            ("src/bridge.rs", "pub(crate) type Out = crate::raw::Raw;\n"),
            (
                "src/api.rs",
                "pub fn id() -> crate::bridge::Out {\n    0\n}\n",
            ),
        ],
        &[("src/raw.rs", "pub(crate) type Raw = u64;\n")],
    );
    changed(&api, "id");
    // 外へ出た鎖が、変えたファイルへ戻る
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod api;\nmod bridge;\n"),
            (
                "src/bridge.rs",
                "pub(crate) type Out = crate::api::Inner;\n",
            ),
            (
                "src/api.rs",
                "pub(crate) type Inner = u8;\n\npub fn run() -> crate::bridge::Out {\n    0\n}\n",
            ),
        ],
        &[(
            "src/api.rs",
            "pub(crate) type Inner = u16;\n\npub fn run() -> crate::bridge::Out {\n    0\n}\n",
        )],
    );
    changed(&api, "run");
    // 子のファイルが `super::` で親の型別名を名指す
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod parent;\n"),
            ("src/parent.rs", "pub mod child;\n\ntype Text = u8;\n"),
            (
                "src/parent/child.rs",
                "use super::Text;\n\npub fn label(_: Text) {}\n",
            ),
        ],
        &[("src/parent.rs", "pub mod child;\n\ntype Text = u16;\n")],
    );
    changed(&api, "label");
    // module の別名を付け替え、変えていない glob を経る
    let lib = |selected: &str| {
        format!(
            "pub mod a {{\n    pub type T = u8;\n}}\npub mod b {{\n    pub type T = u16;\n}}\npub use crate::{selected} as selected;\npub mod api;\npub mod bridge;\n"
        )
    };
    let (old_lib, new_lib) = (lib("a"), lib("b"));
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", &old_lib),
            ("src/bridge.rs", "pub use crate::selected::*;\n"),
            ("src/api.rs", "pub fn f(_: crate::bridge::T) {}\n"),
        ],
        &[("src/lib.rs", &new_lib)],
    );
    changed(&api, "f");
}

/// `#[path]` で開くファイルの公開関数も比べる。
#[test]
fn files_opened_through_path_attributes_are_followed() {
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "#[path = \"impl.rs\"]\npub mod api;\nmod types;\n",
            ),
            ("src/types.rs", "pub(crate) type T = u8;\n"),
            ("src/impl.rs", "pub fn f(_: crate::types::T) {}\n"),
        ],
        &[("src/types.rs", "pub(crate) type T = u16;\n")],
    );
    assert_eq!(
        dependency_reason(&api, "f"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// 型を別のファイルへ移して再公開を付け替えただけなら、変えていない型別名の右辺の中で名指していても
/// 変化にしない。元からあった別の型への付け替えは変化。
#[test]
fn moves_inside_alias_right_hand_sides_are_not_changes() {
    let lib = |from: &str| {
        format!("pub mod a;\npub mod b;\npub mod api;\npub mod bridge;\npub use {from}::X;\n")
    };
    let (old_lib, new_lib) = (lib("a"), lib("b"));
    // 名指す側の型別名の行も変える (見える範囲だけ)
    let bridge = ("src/bridge.rs", "pub type T = crate::X;\n");
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", &old_lib),
            ("src/a.rs", "pub struct X;\n"),
            ("src/b.rs", "pub fn other() {}\n"),
            ("src/bridge.rs", "pub(crate) type T = crate::X;\n"),
            ("src/api.rs", "pub fn f(_: crate::bridge::T) {}\n"),
        ],
        &[
            ("src/lib.rs", &new_lib),
            ("src/a.rs", ""),
            ("src/b.rs", "pub fn other() {}\n\npub struct X;\n"),
            bridge,
        ],
    );
    assert_eq!(dependency_reason(&api, "f"), None, "{api:?}");
    // 移さずに、元からあった別の型へ付け替える
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", &old_lib),
            ("src/a.rs", "pub struct X;\n"),
            ("src/b.rs", "pub struct X;\n"),
            ("src/bridge.rs", "pub(crate) type T = crate::X;\n"),
            ("src/api.rs", "pub fn f(_: crate::bridge::T) {}\n"),
        ],
        &[("src/lib.rs", &new_lib), bridge],
    );
    assert_eq!(
        dependency_reason(&api, "f"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// 束縛を変えても、契約のパスが変わった名前に触れなければ、ほかのファイルを解かない。解けない経路
/// (`cfg_attr` の中の `path`) を持つ公開関数を、無関係な変更で blocking にしない。
#[test]
fn unrelated_unresolvable_paths_stay_textual() {
    let api = api_changes_after(
        &[
            (
                "src/lib.rs",
                "pub mod api;\n#[cfg_attr(windows, path = \"sys_windows.rs\")]\nmod sys;\nmod other;\n",
            ),
            ("src/sys.rs", "pub(crate) struct T;\n"),
            ("src/sys_windows.rs", "pub(crate) struct T;\n"),
            ("src/other.rs", "pub(crate) type Kept = u8;\n"),
            ("src/api.rs", "pub fn f(_: crate::sys::T) {}\n"),
        ],
        |repo| {
            fs::write(
                repo.join("src/other.rs"),
                "pub(crate) type Kept = u8;\npub(crate) type Added = u16;\n",
            )
            .expect("write");
            fs::write(
                repo.join("src/api.rs"),
                "pub fn f(_: crate::sys::T) {\n    let _ = 1;\n}\n",
            )
            .expect("write");
        },
    );
    assert_eq!(dependency_reason(&api, "f"), None, "{api:?}");
}

/// 束縛を変えた crate で、変えた名前を書いていて構文を解けないファイルは比べられないので、比べ切れ
/// なかったと申告する (hook を止める)。
#[test]
fn unparsable_files_naming_changed_bindings_are_reported() {
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod api;\nmod error;\n"),
            (
                "src/error.rs",
                "pub(crate) type Result<T> = std::result::Result<T, String>;\n",
            ),
            (
                "src/api.rs",
                "use crate::error::Result;\n\npub fn run() -> Result<()> {\n    Ok(())\n}\n\nfn broken( {\n",
            ),
        ],
        &[(
            "src/error.rs",
            "pub(crate) type Result<T> = std::result::Result<T, u8>;\n",
        )],
    );
    assert!(
        api.parse_truncations
            .iter()
            .any(|truncation| truncation.reason
                == crate::models::truncation::TruncationReason::RustDependentsIncomplete),
        "{api:?}"
    );
}

/// 内容を変えずに rename したファイル (`#[path]` で読む) の公開関数も、名指す束縛が変われば比べる。
#[test]
fn renamed_files_without_content_changes_are_compared() {
    let api = api_changes_after(
        &[
            ("src/lib.rs", "pub mod api;\nmod types;\n"),
            ("src/types.rs", "pub(crate) type Out = u8;\n"),
            (
                "src/api.rs",
                "pub fn run() -> crate::types::Out {\n    0\n}\n",
            ),
        ],
        |repo| {
            git_in(repo, &["mv", "src/api.rs", "src/renamed.rs"]);
            fs::write(
                repo.join("src/lib.rs"),
                "#[path = \"renamed.rs\"]\npub mod api;\nmod types;\n",
            )
            .expect("write");
            fs::write(repo.join("src/types.rs"), "pub(crate) type Out = u16;\n").expect("write");
        },
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// 2 つの module に使われるファイル (module を 1 つに決められない) を変えたとき、変わった束縛に触れる
/// 公開関数は、同じと言えないので blocking に残す。
#[test]
fn files_used_by_several_modules_stay_unresolved() {
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "mod types;\n#[path = \"shared.rs\"]\npub mod a;\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
            ("src/types.rs", "pub(crate) type Out = u8;\n"),
            (
                "src/shared.rs",
                "pub fn run() -> crate::types::Out {\n    0\n}\n",
            ),
        ],
        &[
            ("src/types.rs", "pub(crate) type Out = u16;\n"),
            (
                "src/shared.rs",
                "pub fn run() -> crate::types::Out {\n    // note\n    0\n}\n",
            ),
        ],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyUnresolved),
        "{api:?}"
    );
}

/// 条件で分けた inline の module と別のファイルの module は、どちらの枝も比べる。
#[test]
fn conditional_inline_and_file_modules_are_both_followed() {
    let lib = "#[cfg(not(windows))]\nmod selected {\n    pub(crate) type T = u8;\n}\n#[cfg(windows)]\n#[path = \"win.rs\"]\nmod selected;\n\npub fn run() -> selected::T {\n    0\n}\n";
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", lib),
            ("src/win.rs", "pub(crate) type T = u16;\n"),
        ],
        &[("src/win.rs", "pub(crate) type T = u32;\n")],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// 移した先に差分に無いファイルの同じ名前の型が元からあれば、移動でなく付け替え (変化)。
#[test]
fn moves_onto_existing_definitions_outside_the_diff_are_changes() {
    let lib = |path: &str, from: &str| {
        format!(
            "mod a;\n#[path = \"{path}\"]\nmod b;\nmod bridge;\npub mod api;\npub(crate) use {from}::X;\n"
        )
    };
    let (old_lib, new_lib) = (lib("b_old.rs", "a"), lib("b_new.rs", "b"));
    let api = api_changes_after(
        &[
            ("src/lib.rs", &old_lib),
            ("src/a.rs", "pub struct X;\n"),
            ("src/b_old.rs", "pub struct X;\n"),
            ("src/bridge.rs", "pub(crate) type Out = crate::X;\n"),
            (
                "src/api.rs",
                "pub fn run() -> crate::bridge::Out {\n    panic!()\n}\n",
            ),
        ],
        |repo| {
            fs::write(repo.join("src/lib.rs"), &new_lib).expect("write");
            fs::write(repo.join("src/a.rs"), "").expect("write");
            fs::write(repo.join("src/b_new.rs"), "pub struct X;\n").expect("write");
        },
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// `#[path]` で `src/` の外に置いたファイルの束縛の変化も追う。
#[test]
fn bindings_in_files_outside_src_are_followed() {
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "#[path = \"../types.rs\"]\nmod types;\n\npub fn run() -> types::Out {\n    0\n}\n",
            ),
            ("types.rs", "pub(crate) type Out = u8;\n"),
        ],
        &[("types.rs", "pub(crate) type Out = u16;\n")],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// glob が持ち込む module を経るパス (`use crate::names::*;` の `selected::T`) も解く。
#[test]
fn modules_brought_in_by_globs_are_followed() {
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "mod names;\npub mod api;\n"),
            (
                "src/names.rs",
                "pub(crate) mod selected {\n    pub(crate) type T = u8;\n}\n",
            ),
            (
                "src/api.rs",
                "use crate::names::*;\n\npub fn run(_: selected::T) {}\n",
            ),
        ],
        &[(
            "src/names.rs",
            "pub(crate) mod selected {\n    pub(crate) type T = u16;\n}\n",
        )],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
}

/// 束縛の変化に触れない、元から解けない束縛や読み方の決まった module を、無関係な変更で blocking に
/// しない (`cfg_attr` の中の `path`、`#[path]` で開いたファイルの中の `mod`、外部 crate と同じ名前の
/// 取り込み、`use super::X;` を持つ子を glob で再公開する親)。
#[test]
fn unrelated_changes_do_not_block_existing_bindings() {
    // 無関係な `use` を足す
    let api = api_changes_after(
        &[
            (
                "src/lib.rs",
                "#[cfg_attr(windows, path = \"sys_windows.rs\")]\nmod sys;\npub mod api;\n",
            ),
            ("src/sys.rs", "pub(crate) type T = u8;\n"),
            ("src/sys_windows.rs", "pub(crate) type T = u8;\n"),
            ("src/api.rs", "pub fn run(_: crate::sys::T) {}\n"),
        ],
        |repo| {
            fs::write(
                repo.join("src/lib.rs"),
                "#[cfg_attr(windows, path = \"sys_windows.rs\")]\nmod sys;\npub mod api;\n\nuse std::collections::HashMap as _Unused;\n",
            )
            .expect("write");
            fs::write(
                repo.join("src/api.rs"),
                "pub fn run(_: crate::sys::T) {\n    let _ = 1;\n}\n",
            )
            .expect("write");
        },
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
    // `#[path]` で開いたファイルの中の `mod` は、そのファイルのディレクトリから探す
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "#[path = \"outer.rs\"]\npub mod outer;\nmod other;\n",
            ),
            ("src/outer.rs", "mod leaf;\n\npub fn run(_: leaf::T) {}\n"),
            ("src/leaf.rs", "pub(super) type T = u8;\n"),
            ("src/other.rs", "pub(crate) type T = u8;\n"),
        ],
        &[
            ("src/other.rs", "pub(crate) type T = u16;\n"),
            (
                "src/outer.rs",
                "mod leaf;\n\npub fn run(_: leaf::T) {\n    let _ = 1;\n}\n",
            ),
        ],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
    // 外部 crate と同じ名前の項目を取り込む use (`use anyhow::{Result, anyhow};`) は、経路の先頭の
    // `anyhow` を自分で引き直さない (本体だけの変更)
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "pub mod api;
",
            ),
            (
                "src/api.rs",
                "use anyhow::{Result, anyhow};

pub fn run() -> Result<()> {
    Ok(())
}
",
            ),
        ],
        &[(
            "src/api.rs",
            "use anyhow::{Result, anyhow};

pub fn run() -> Result<()> {
    let _ = anyhow!(\"x\");
    Ok(())
}
",
        )],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
    // glob で再公開する親の名前を、別の子が非公開の `use super::Shared;` で取り込んでも、親の glob
    // からそれを引き直さない (ほかの module で同じ名前を足して、`Shared` を比べる対象にする)
    let base = [
        (
            "src/lib.rs",
            "mod a;
mod b;
mod c;
pub mod api;
pub(crate) use a::*;
pub(crate) use b::*;
",
        ),
        (
            "src/a.rs",
            "pub(crate) struct Shared;
",
        ),
        (
            "src/b.rs",
            "use super::Shared;

pub(crate) fn helper(_: Shared) {}
",
        ),
        (
            "src/c.rs",
            "pub(crate) fn other() {}
",
        ),
        (
            "src/api.rs",
            "use crate::Shared;

pub fn run(_: Shared) {}
",
        ),
    ];
    let api = api_changes_of_files(
        &base,
        &[
            (
                "src/c.rs",
                "pub(crate) fn other() {}

pub(crate) struct Shared;
",
            ),
            (
                "src/api.rs",
                "use crate::Shared;

pub fn run(_: Shared) {
    let _ = 1;
}
",
            ),
        ],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
}

/// 2 回目のレビューの指摘: 定数の右辺、変えたファイルの `#[path]`、`#[path]` で置いた module の配下、
/// glob が持ち込む module を経る名前付きの use、`src` を含む外の経路。
#[test]
fn bindings_through_constants_paths_and_globbed_modules_are_followed() {
    let changed = |api: &crate::models::review::ApiChanges, name: &str| {
        assert_eq!(
            dependency_reason(api, name),
            Some(Reason::RustSignatureDependencyChanged),
            "{name}: {api:?}"
        );
    };
    // 変えていない定数の右辺を経る配列の長さ
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "mod n;\nmod bridge;\npub mod api;\n"),
            ("src/n.rs", "pub(crate) const N: usize = 1;\n"),
            (
                "src/bridge.rs",
                "pub(crate) const M: usize = crate::n::N;\n",
            ),
            ("src/api.rs", "pub fn run(_: [u8; crate::bridge::M]) {}\n"),
        ],
        &[("src/n.rs", "pub(crate) const N: usize = 2;\n")],
    );
    changed(&api, "run");
    // 変えたファイルが `#[path]` で `src/` の外に置く module
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "#[path = \"../api.rs\"]\npub mod api;\nmod types;\n",
            ),
            ("src/types.rs", "pub(crate) type Out = u8;\n"),
            ("api.rs", "pub fn run() -> crate::types::Out {\n    0\n}\n"),
        ],
        &[
            (
                "src/lib.rs",
                "#[path = \"../api.rs\"]\npub mod api;\nmod types;\n// note\n",
            ),
            ("src/types.rs", "pub(crate) type Out = u16;\n"),
        ],
    );
    changed(&api, "run");
    // `#[path]` で別の module の配下に置いたファイルが、親の glob の変わった module を名指す
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "pub mod a;\n"),
            (
                "src/a.rs",
                "pub mod selected {\n    pub use std::fmt::*;\n}\n#[path = \"api_impl.rs\"]\npub mod api;\n",
            ),
            (
                "src/api_impl.rs",
                "pub fn run(_: super::selected::Error) {}\n",
            ),
        ],
        &[(
            "src/a.rs",
            "pub mod selected {\n    pub use std::io::*;\n}\n#[path = \"api_impl.rs\"]\npub mod api;\n",
        )],
    );
    changed(&api, "run");
    // glob が持ち込む module を、名前付きの use で経る
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "mod names;\npub mod api;\n"),
            (
                "src/names.rs",
                "pub(crate) mod selected {\n    pub(crate) type T = u8;\n}\n",
            ),
            (
                "src/api.rs",
                "use crate::names::*;\nuse selected::T;\n\npub fn run(_: T) {}\n",
            ),
        ],
        &[(
            "src/names.rs",
            "pub(crate) mod selected {\n    pub(crate) type T = u16;\n}\n",
        )],
    );
    changed(&api, "run");
    // `src` という名前のディレクトリを含む、package の外の `#[path]`
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "#[path = \"../generated/src/types.rs\"]\nmod types;\n\npub fn run() -> types::Out {\n    0\n}\n",
            ),
            ("generated/src/types.rs", "pub(crate) type Out = u8;\n"),
        ],
        &[("generated/src/types.rs", "pub(crate) type Out = u16;\n")],
    );
    changed(&api, "run");
}

/// `pub(super) use super::X;` を glob で再公開する親 (合法な循環) は、無関係な変更で blocking にしない。
#[test]
fn reexport_cycles_through_globs_do_not_block() {
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "mod a;\nmod b;\nmod c;\npub mod api;\npub(crate) use a::*;\npub(crate) use b::*;\n",
            ),
            ("src/a.rs", "pub struct Shared;\n"),
            ("src/b.rs", "pub(super) use super::Shared;\n"),
            ("src/c.rs", "pub(crate) fn other() {}\n"),
            ("src/api.rs", "pub fn run(_: crate::Shared) {}\n"),
        ],
        &[(
            "src/c.rs",
            "pub(crate) fn other() {}\n\npub(crate) struct Shared;\n",
        )],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
}

/// 3 回目のレビューの指摘: 同じファイルを 2 つの module として読む (`#[path]`) とき、別の module として
/// 引く束縛を循環とみなさない。規約の場所と `#[path]` の両方で使うファイルは、どの module として比べるか
/// 決められないので unresolved にする。
#[test]
fn files_read_as_several_modules_are_compared_per_module() {
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "type T = u8;\nmod shared;\nmod bridge;\npub mod api;\n",
            ),
            ("src/shared.rs", "pub(crate) type Out = super::T;\n"),
            (
                "src/bridge.rs",
                "type T = crate::shared::Out;\n#[path = \"shared.rs\"]\nmod nested;\npub(crate) use nested::Out;\n",
            ),
            ("src/api.rs", "pub fn run(_: crate::bridge::Out) {}\n"),
        ],
        &[(
            "src/lib.rs",
            "type T = u16;\nmod shared;\nmod bridge;\npub mod api;\n",
        )],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "type T = u8;\npub mod shared;\npub mod bridge;\n",
            ),
            ("src/shared.rs", "pub fn run(_: super::T) {}\n"),
            (
                "src/bridge.rs",
                "type T = u8;\n#[path = \"shared.rs\"]\npub mod nested;\n",
            ),
        ],
        &[(
            "src/bridge.rs",
            "type T = u16;\n#[path = \"shared.rs\"]\npub mod nested;\n",
        )],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyUnresolved),
        "{api:?}"
    );
}

/// glob は、取り込む側から見えない宣言 (非公開の型別名) を持ち込まない。外の crate への glob が多い
/// ファイルの本体だけの変更も、すぐに終わって報告しない。
#[test]
fn globs_bring_in_only_visible_declarations() {
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "mod a;\nmod b;\npub use a::*;\npub use b::*;\n\npub fn run(_: Text) {}\n",
            ),
            ("src/a.rs", "pub type Text = String;\n"),
            ("src/b.rs", "type Text = u8;\n"),
        ],
        &[("src/b.rs", "type Text = u16;\n")],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
    let globs = "use std::{\n    cell::*, cmp::*, collections::*, convert::*, fmt::*, hash::*, io::*, iter::*,\n};\nuse std::path::PathBuf;\n";
    let api = api_changes_of_files(
        &[(
            "src/lib.rs",
            &format!("{globs}\npub fn run(_: PathBuf) {{\n    let _ = 0;\n}}\n"),
        )],
        &[(
            "src/lib.rs",
            &format!("{globs}\npub fn run(_: PathBuf) {{\n    let _ = 1;\n}}\n"),
        )],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
}

/// 4 回目のレビューの指摘: 見える範囲を広げて glob が持ち込むようになる型別名、中身が別のファイルの
/// module の別名、祖先の module の別名を経る名指し。
#[test]
fn visibility_and_module_aliases_are_followed() {
    let changed = |api: &crate::models::review::ApiChanges, name: &str| {
        assert_eq!(
            dependency_reason(api, name),
            Some(Reason::RustSignatureDependencyChanged),
            "{name}: {api:?}"
        );
    };
    // `pub(super)` から `pub(crate)` に広げると、glob で取り込む側の名前が prelude から型別名に変わる
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "mod internal;\npub mod api;\n"),
            (
                "src/internal.rs",
                "pub mod names {\n    pub(super) type String = std::path::PathBuf;\n}\n",
            ),
            (
                "src/api.rs",
                "use crate::internal::names::*;\n\npub fn run(_: String) {}\n",
            ),
        ],
        &[(
            "src/internal.rs",
            "pub mod names {\n    pub(crate) type String = std::path::PathBuf;\n}\n",
        )],
    );
    changed(&api, "run");
    // 中身が別のファイルの module の別名
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "mod types;\nuse types as selected;\npub mod api;\n",
            ),
            ("src/types.rs", "pub(crate) type T = u8;\n"),
            ("src/api.rs", "pub fn run(_: crate::selected::T) {}\n"),
        ],
        &[("src/types.rs", "pub(crate) type T = u16;\n")],
    );
    changed(&api, "run");
    // 祖先の module の別名を経る
    let api = api_changes_of_files(
        &[
            (
                "src/lib.rs",
                "pub mod outer;\npub use crate::outer as facade;\npub mod api;\n",
            ),
            (
                "src/outer.rs",
                "pub mod types {\n    pub use std::fmt::*;\n}\n",
            ),
            (
                "src/api.rs",
                "pub fn run(_: crate::facade::types::Error) {}\n",
            ),
        ],
        &[(
            "src/outer.rs",
            "pub mod types {\n    pub use std::io::*;\n}\n",
        )],
    );
    changed(&api, "run");
}

/// 5 回目のレビューの指摘: 単一名の `use` が同じ module の型別名を指すときも追い、テストの module の
/// 中の名前の変化で、無関係な解けない経路を比べない。
#[test]
fn single_name_uses_and_test_modules() {
    let api = api_changes_of_files(
        &[
            ("src/lib.rs", "mod types;\npub mod api;\n"),
            (
                "src/types.rs",
                "pub type Raw = u8;\npub(crate) use Raw as Out;\n",
            ),
            ("src/api.rs", "pub fn run(_: crate::types::Out) {}\n"),
        ],
        &[(
            "src/types.rs",
            "pub type Raw = u16;\npub(crate) use Raw as Out;\n",
        )],
    );
    assert_eq!(
        dependency_reason(&api, "run"),
        Some(Reason::RustSignatureDependencyChanged),
        "{api:?}"
    );
    let base = [
        (
            "src/lib.rs",
            "mod helper;\npub mod api;\n#[cfg_attr(windows, path = \"platform_windows.rs\")]\nmod platform;\n",
        ),
        (
            "src/platform.rs",
            "pub(crate) type Result<T> = std::result::Result<T, u8>;\n",
        ),
        (
            "src/platform_windows.rs",
            "pub(crate) type Result<T> = std::result::Result<T, u8>;\n",
        ),
        (
            "src/api.rs",
            "pub fn run() -> crate::platform::Result<()> {\n    Ok(())\n}\n",
        ),
        (
            "src/helper.rs",
            "#[cfg(test)]\nmod tests {\n    use std::fmt::Result;\n}\n",
        ),
    ];
    // テストの module の中の名前を変える
    let api = api_changes_of_files(
        &base,
        &[(
            "src/helper.rs",
            "#[cfg(test)]\nmod tests {\n    use std::io::Result;\n}\n",
        )],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
    assert!(api.parse_truncations.is_empty(), "{api:?}");
    // テストの module を足す
    let api = api_changes_of_files(
        &base,
        &[(
            "src/helper.rs",
            "#[cfg(test)]\nmod tests {\n    use std::fmt::Result;\n}\n\n#[cfg(test)]\nmod more_tests {\n    use super::*;\n}\n",
        )],
    );
    assert_eq!(dependency_reason(&api, "run"), None, "{api:?}");
    assert!(api.parse_truncations.is_empty(), "{api:?}");
}
