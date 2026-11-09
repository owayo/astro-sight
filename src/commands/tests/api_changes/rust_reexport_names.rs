//! Rust の `pub use` が作る公開の名前の有る設定と削除 (Issue #306・#318)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

use crate::models::review::{ApiChanges, ApiModificationUnverifiedReason, CfgAvailability};
use crate::models::truncation::TruncationReason;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\ny = []\n";

/// `files` をコミットし、`changes` に書き換えたときの API の差分。
fn api_changes(files: &[(&str, &str)], changes: &[(&str, &str)]) -> ApiChanges {
    api_changes_with(CARGO, files, changes)
}

/// manifest `manifest` の package で、`files` をコミットし、`changes` に書き換えたときの API の差分。
fn api_changes_with(
    manifest: &str,
    files: &[(&str, &str)],
    changes: &[(&str, &str)],
) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut all = vec![("Cargo.toml", manifest)];
    all.extend_from_slice(files);
    git_commit_files(repo, &all, "base");
    for (path, content) in changes {
        let path = repo.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, content).expect("write");
    }
    detect_api_changes_from_worktree(repo)
}

/// `name` の区分 (rm・rm_dead・add・mod・none) と、添えた有る条件。
fn classify(api: &ApiChanges, name: &str) -> (&'static str, Option<CfgAvailability>) {
    let find = |symbols: &[crate::models::review::ApiSymbol]| {
        symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .map(|symbol| symbol.cfg_availability.clone())
    };
    if let Some(cfg) = find(&api.removed) {
        ("rm", cfg)
    } else if let Some(cfg) = find(&api.removed_dead) {
        ("rm_dead", cfg)
    } else if let Some(cfg) = find(&api.added) {
        ("add", cfg)
    } else if api.modified.iter().any(|change| change.name == name) {
        ("mod", None)
    } else {
        ("none", None)
    }
}

/// 再公開した名前を探し切れなかったことを申告したか。
fn incomplete(api: &ApiChanges) -> bool {
    api.parse_truncations
        .iter()
        .any(|truncation| truncation.reason == TruncationReason::RustReexportsIncomplete)
}

/// 比べられない変更 (blocking) か。
fn unverified(api: &ApiChanges, name: &str) -> bool {
    api.modified.iter().any(|change| {
        change.name == name
            && change
                .unverified
                .as_ref()
                .map(|unverified| unverified.reason)
                == Some(ApiModificationUnverifiedReason::RustCfgAvailabilityUnverified)
    })
}

fn cfg(old: &[&[&str]], new: &[&[&str]]) -> Option<CfgAvailability> {
    let side = |side: &[&[&str]]| -> Vec<Vec<String>> {
        side.iter()
            .map(|attributes| attributes.iter().map(|text| (*text).to_owned()).collect())
            .collect()
    };
    Some(CfgAvailability {
        old: side(old),
        new: side(new),
    })
}

const X: &str = "cfg(feature = \"x\")";
const DETAIL: (&str, &str) = ("src/detail.rs", "pub fn make() -> u32 {\n    1\n}\n");

/// Issue #306 の再現: `pub use` に `#[cfg]` を足すと、再公開した名前が消える設定がある。
#[test]
fn reexport_cfg_added_is_a_removal() {
    let api = api_changes(
        &[
            ("src/lib.rs", "mod detail;\n\npub use detail::make;\n"),
            DETAIL,
        ],
        &[(
            "src/lib.rs",
            "mod detail;\n\n#[cfg(feature = \"x\")]\npub use detail::make;\n",
        )],
    );
    let (bucket, availability) = classify(&api, "make");
    assert!(matches!(bucket, "rm" | "rm_dead"), "{api:?}");
    assert_eq!(availability, cfg(&[&[]], &[&[X]]), "{api:?}");
    // 同値な書き換えは報告しない
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\n\n#[cfg(all(feature = \"x\", feature = \"y\"))]\npub use detail::make;\n",
            ),
            DETAIL,
        ],
        &[(
            "src/lib.rs",
            "mod detail;\n\n#[cfg(feature = \"x\")]\n#[cfg(feature = \"y\")]\npub use detail::make;\n",
        )],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
    // 条件を外すと増える設定
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\n\n#[cfg(feature = \"x\")]\npub use detail::make;\n",
            ),
            DETAIL,
        ],
        &[("src/lib.rs", "mod detail;\n\npub use detail::make;\n")],
    );
    assert_eq!(
        classify(&api, "make"),
        ("add", cfg(&[&[X]], &[&[]])),
        "{api:?}"
    );
}

/// Issue #318 の再現: `pub use` を消すか公開範囲を狭めると、再公開した名前の削除。新しい `pub use` は追加。
#[test]
fn reexport_removed_or_narrowed_is_a_removal() {
    let base = [
        ("src/lib.rs", "mod detail;\n\npub use detail::make;\n"),
        DETAIL,
    ];
    let api = api_changes(&base, &[("src/lib.rs", "mod detail;\n")]);
    assert!(
        matches!(classify(&api, "make"), ("rm" | "rm_dead", None)),
        "{api:?}"
    );
    let api = api_changes(
        &base,
        &[(
            "src/lib.rs",
            "mod detail;\n\npub(crate) use detail::make;\n",
        )],
    );
    assert!(
        matches!(classify(&api, "make"), ("rm" | "rm_dead", None)),
        "{api:?}"
    );
    let api = api_changes(
        &[("src/lib.rs", "mod detail;\n"), DETAIL],
        &[("src/lib.rs", "mod detail;\n\npub use detail::make;\n")],
    );
    assert_eq!(classify(&api, "make"), ("add", None), "{api:?}");
    // どの設定でも無かった再公開を消しても報告しない
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\n\n#[cfg(any())]\npub use detail::make as build;\n",
            ),
            DETAIL,
        ],
        &[("src/lib.rs", "mod detail;\n")],
    );
    assert_eq!(classify(&api, "build"), ("none", None), "{api:?}");
}

/// 別の公開の名前が残ることでは相殺しない。同じ公開の名前の別の経路 (glob) が残れば削除にしない。
#[test]
fn other_routes_to_the_same_name_count_but_other_names_do_not() {
    // 公開の module の項目が残っても、別名 `build` は消える
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub mod detail;\n\npub use detail::make as build;\n",
            ),
            DETAIL,
        ],
        &[("src/lib.rs", "pub mod detail;\n")],
    );
    assert!(
        matches!(classify(&api, "build").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    // glob が同じ項目を持ち込み続ける
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\n\npub use detail::*;\npub use detail::make;\n",
            ),
            DETAIL,
        ],
        &[("src/lib.rs", "mod detail;\n\npub use detail::*;\n")],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
    // 先の違う glob が重なると、名前を決められない
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod a;\nmod b;\n\npub use a::*;\npub use b::*;\npub use a::make;\n",
            ),
            ("src/a.rs", "pub fn make() {}\n"),
            ("src/b.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            "mod a;\nmod b;\n\npub use a::*;\npub use b::*;\n",
        )],
    );
    assert!(unverified(&api, "make"), "{api:?}");
}

/// 名前空間: 同じ名前の型 (module) が残っても、値 (関数) の再公開は消える。型だけの `self` の取り込みは、
/// 先が module だけなら変化にしない。
#[test]
fn namespaces_are_compared_separately() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\npub mod item {}\n\npub use detail::item;\n",
            ),
            ("src/detail.rs", "pub fn item() {}\n"),
        ],
        &[("src/lib.rs", "mod detail;\npub mod item {}\n")],
    );
    assert!(
        matches!(classify(&api, "item").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    let api = api_changes(
        &[
            ("src/lib.rs", "mod detail;\n\npub use detail::item;\n"),
            ("src/detail.rs", "pub mod item {}\n"),
        ],
        &[(
            "src/lib.rs",
            "mod detail;\n\npub use detail::item::{self};\n",
        )],
    );
    assert_eq!(classify(&api, "item"), ("none", None), "{api:?}");
}

/// 非公開の module を通る再公開の連鎖と、外側の条件。
#[test]
fn reexport_chains_follow_the_outer_conditions() {
    // 非公開の module の再公開に条件を足すと、外の名前が消える
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\nmod bridge;\n\npub use bridge::make;\n",
            ),
            DETAIL,
            ("src/bridge.rs", "pub use crate::detail::make;\n"),
        ],
        &[(
            "src/bridge.rs",
            "#[cfg(feature = \"x\")]\npub use crate::detail::make;\n",
        )],
    );
    assert!(
        matches!(classify(&api, "make").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    // 外側が元から同じ条件なら、外の名前は変わらない
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\nmod facade;\n\n#[cfg(feature = \"x\")]\npub use facade::make as build;\n",
            ),
            DETAIL,
            ("src/facade.rs", "pub use crate::detail::make;\n"),
        ],
        &[(
            "src/facade.rs",
            "#[cfg(feature = \"x\")]\npub use crate::detail::make;\n",
        )],
    );
    assert_eq!(classify(&api, "build"), ("none", None), "{api:?}");
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// module ごとの再公開と inline module の中の再公開、非公開の inline module の中の再公開。
#[test]
fn module_reexports_and_inline_modules_are_followed() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\nmod facade;\n\npub use facade::inner as api;\n",
            ),
            DETAIL,
            (
                "src/facade.rs",
                "pub mod inner {\n    pub use crate::detail::make as release;\n}\n",
            ),
        ],
        &[("src/facade.rs", "pub mod inner {}\n")],
    );
    assert!(
        matches!(classify(&api, "release").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    // module の再公開そのものを消す
    let api = api_changes(
        &[(
            "src/lib.rs",
            "mod hidden {\n    pub mod detail {\n        pub fn make() {}\n    }\n}\n\npub use hidden::detail as d;\n",
        )],
        &[(
            "src/lib.rs",
            "mod hidden {\n    pub mod detail {\n        pub fn make() {}\n    }\n}\n",
        )],
    );
    assert!(matches!(classify(&api, "d").0, "rm" | "rm_dead"), "{api:?}");
    // 非公開の inline module の中の再公開は公開の名前を作らない
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\n\nmod hidden {\n    pub use crate::detail::make as build;\n}\n",
            ),
            DETAIL,
        ],
        &[("src/lib.rs", "mod detail;\n\nmod hidden {}\n")],
    );
    assert_eq!(classify(&api, "build"), ("none", None), "{api:?}");
}

/// 外部 crate の glob や項目の位置のマクロが同じ名前を持ち込み得るなら、決められない。ふつうの
/// `use` (取り込み) の変更では比べない。
#[test]
fn unknown_routes_and_plain_imports() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\n\npub use std::collections::*;\npub use detail::make;\n",
            ),
            DETAIL,
        ],
        &[(
            "src/lib.rs",
            "mod detail;\n\npub use std::collections::*;\n",
        )],
    );
    assert!(unverified(&api, "make"), "{api:?}");
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod detail;\nuse std::fmt;\n\npub use detail::make;\n\npub fn show(_: &dyn fmt::Debug) {}\n",
            ),
            DETAIL,
        ],
        &[(
            "src/lib.rs",
            "mod detail;\nuse std::fmt::{self, Debug};\n\npub use detail::make;\n\npub fn show(_: &dyn Debug) {}\n",
        )],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
    assert!(
        !api.parse_truncations
            .iter()
            .any(|truncation| truncation.reason == TruncationReason::RustReexportsIncomplete),
        "{api:?}"
    );
}

/// 名前空間ごとの変化をまとめるとき、ほかの名前空間の増える設定で消える設定を落とさない。
#[test]
fn namespace_changes_keep_the_lost_configurations() {
    let modules = "mod t {\n    pub struct Item {}\n}\nmod v {\n    pub fn Item() {}\n}\n";
    let old = format!("{modules}\n#[cfg(feature = \"x\")]\npub use t::Item;\npub use v::Item;\n");
    let new = format!("{modules}\npub use t::Item;\n#[cfg(feature = \"x\")]\npub use v::Item;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(
        matches!(classify(&api, "Item").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// 非公開の別名を差し替えると、それを経路に使う glob の再公開の名前が変わる (同じファイルの別の module
/// の再公開も)。
#[test]
fn private_alias_changes_reach_the_globs_through_them() {
    let modules = "mod a {\n    pub fn make() {}\n}\nmod b {\n    pub fn other() {}\n}\n";
    for reexport in [
        "pub use selected::*;\n",
        "pub mod api {\n    pub use crate::selected::*;\n}\n",
    ] {
        let old = format!("{modules}use crate::a as selected;\n{reexport}");
        let new = format!("{modules}use crate::b as selected;\n{reexport}");
        let api = api_changes(
            &[("src/lib.rs", old.as_str())],
            &[("src/lib.rs", new.as_str())],
        );
        assert!(
            matches!(classify(&api, "make").0, "rm" | "rm_dead"),
            "{reexport}: {api:?}"
        );
        assert_eq!(classify(&api, "other").0, "add", "{reexport}: {api:?}");
    }
}

/// 条件付きの別名は、条件ごとの先を全てたどる。起点が識別子の別名 (`use a as selected;`) もたどる。
#[test]
fn conditional_aliases_are_followed_with_their_conditions() {
    let modules = "mod a {\n    pub fn one() {}\n}\nmod b {\n    pub fn two() {}\n}\n";
    let aliases = "#[cfg(feature = \"x\")]\nuse crate::a as selected;\n#[cfg(not(feature = \"x\"))]\nuse crate::b as selected;\n";
    let old = format!("{modules}{aliases}pub use selected::*;\n");
    let new = format!("{modules}{aliases}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(
        matches!(classify(&api, "one"), ("rm" | "rm_dead", None)),
        "{api:?}"
    );
    assert!(
        matches!(classify(&api, "two"), ("rm" | "rm_dead", None)),
        "{api:?}"
    );
    let old = format!("{modules}use a as selected;\npub use selected::*;\n");
    let new = format!("{modules}use a as selected;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(
        matches!(classify(&api, "one").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    assert!(!incomplete(&api), "{api:?}");
}

/// module ごとの再公開の先の子 module が別のファイルでも、公開の経路をたどる。
#[test]
fn file_children_of_reexported_modules_are_public() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod facade;\nmod detail {\n    pub fn make() {}\n}\n\npub use facade::inner as api;\n",
            ),
            ("src/facade.rs", "pub mod inner {\n    pub mod child;\n}\n"),
            (
                "src/facade/inner/child.rs",
                "pub use crate::detail::make as release;\n",
            ),
        ],
        &[("src/facade/inner/child.rs", "\n")],
    );
    assert!(
        matches!(classify(&api, "release").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// マクロと enum の variant の再公開も比べる。
#[test]
fn macros_and_enum_variants() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "#[macro_export]\nmacro_rules! m {\n    () => {};\n}\n\npub use crate::m as exposed;\n",
        )],
        &[(
            "src/lib.rs",
            "#[macro_export]\nmacro_rules! m {\n    () => {};\n}\n",
        )],
    );
    assert!(
        matches!(classify(&api, "exposed").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub enum E {\n    V,\n}\n\npub use self::E::V as exposed;\n",
        )],
        &[("src/lib.rs", "pub enum E {\n    V,\n}\n")],
    );
    assert!(
        matches!(classify(&api, "exposed").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    // enum の glob の追加は、variant の追加 (列挙できる)
    let api = api_changes(
        &[("src/lib.rs", "pub enum E {\n    V,\n}\n")],
        &[(
            "src/lib.rs",
            "pub enum E {\n    V,\n}\n\npub use self::E::*;\n",
        )],
    );
    assert_eq!(classify(&api, "V").0, "add", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// 見える範囲を問わず、名前の解決に加わる glob の重なりは決められない。同じ宣言を指す 2 つの経路や、
/// 同時に効かない glob は重ならない。
#[test]
fn glob_conflicts_follow_declarations_and_conditions() {
    let two = "mod a {\n    pub fn make() {}\n}\nmod b {\n    pub fn make() {}\n}\n";
    let old = format!("{two}pub use a::*;\npub(crate) use b::*;\npub use a::make;\n");
    let new = format!("{two}pub use a::*;\npub(crate) use b::*;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "make"), "{api:?}");
    let same = "mod detail {\n    pub fn make() {}\n}\nmod a {\n    pub use crate::detail::make;\n}\nmod b {\n    pub use crate::detail::make;\n}\n";
    let old = format!("{same}pub use a::*;\npub use b::*;\npub use a::make;\n");
    let new = format!("{same}pub use a::*;\npub use b::*;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
    let globs =
        "#[cfg(feature = \"x\")]\npub use a::*;\n#[cfg(not(feature = \"x\"))]\npub use b::*;\n";
    let old = format!("{two}{globs}pub use a::make;\n");
    let new = format!("{two}{globs}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// 場所を決められない module を通る再公開の削除は、黙って見逃さない。
#[test]
fn reexports_through_unknown_placements_are_not_missed() {
    let declaration = "#[cfg_attr(feature = \"x\", path = \"other.rs\")]\nmod detail;\n";
    let old = format!("{declaration}\npub use detail::make;\n");
    let api = api_changes(
        &[
            ("src/lib.rs", old.as_str()),
            DETAIL,
            ("src/other.rs", "pub fn make() -> u32 {\n    2\n}\n"),
        ],
        &[("src/lib.rs", declaration)],
    );
    assert!(
        classify(&api, "make").0 != "none" || incomplete(&api),
        "{api:?}"
    );
}

/// 同じ先の glob が重なる長い連鎖でも、探索の量が増えすぎない。
#[test]
fn repeated_glob_chains_stay_cheap() {
    let mut new = String::from("mod m0 {\n    pub fn make() {}\n}\n");
    for level in 1..=30 {
        let previous = level - 1;
        new.push_str(&format!(
            "mod m{level} {{\n    pub use crate::m{previous}::*;\n    pub use crate::m{previous}::*;\n}}\n"
        ));
    }
    let old = format!("{new}pub use m30::*;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(
        matches!(classify(&api, "make").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// 外へ名前を出さない外部 crate の glob の整理は、探し切れなかったことにしない。
#[test]
fn hidden_external_globs_are_not_incomplete() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "mod hidden {\n    pub(crate) use std::collections::*;\n}\n",
        )],
        &[("src/lib.rs", "mod hidden {}\n")],
    );
    assert!(!incomplete(&api), "{api:?}");
    let api = api_changes(
        &[(
            "src/lib.rs",
            "#[cfg(any())]\npub use std::collections::*;\n\npub fn keep() {}\n",
        )],
        &[("src/lib.rs", "pub fn keep() {}\n")],
    );
    assert!(!incomplete(&api), "{api:?}");
}

/// 1 つの use tree の中の末尾の `self` は、その枝だけを型の取り込みにする。
#[test]
fn self_imports_only_restrict_their_own_branch() {
    let modules = "mod a {\n    pub mod item {}\n}\nmod b {\n    pub fn item() {}\n}\n";
    let old = format!("{modules}pub use {{a::item, b::item}};\n");
    let new = format!("{modules}pub use {{a::item::{{self}}, b::item}};\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "item"), ("none", None), "{api:?}");
}

/// glob の先の `pub(crate)` の項目は、外から使える名前にならない。
#[test]
fn restricted_items_behind_globs_are_not_public() {
    let rest = "\nfn caller() {\n    detail::make();\n}\n";
    let detail = "mod detail {\n    pub(crate) fn make() {}\n}\n";
    let old = format!("{detail}\npub use detail::*;\n{rest}");
    let new = format!("{detail}{rest}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// 増えた設定の名前も、同じファイルの中から呼ぶなら追加にしない (#291 と同じ絞り込み)。
#[test]
fn gained_names_called_in_the_same_file_are_not_added() {
    let detail = "mod detail {\n    pub fn make() {}\n}\n";
    let caller = "\n#[cfg(feature = \"x\")]\nfn caller() {\n    make();\n}\n";
    let old = format!("{detail}\n#[cfg(feature = \"x\")]\npub use detail::make;\n{caller}");
    let new = format!("{detail}\npub use detail::make;\n{caller}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// 名前の指す宣言そのものの追加を報告するなら、再公開した名前の追加を重ねない。削除は、宣言と同じ名前で
/// 同じか強い区分の宣言の削除が残るときだけ重ねない (別名の削除は、別名の参照で区分するので残す)。
#[test]
fn names_of_added_or_removed_declarations_are_not_repeated() {
    let lib = "mod value;\n\npub use value::Amount as Renamed;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/value.rs", "pub struct Amount;\n"),
        ],
        &[("src/lib.rs", "mod value;\n"), ("src/value.rs", "\n")],
    );
    assert!(
        matches!(classify(&api, "Amount").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    assert!(
        matches!(classify(&api, "Renamed").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    // 同じ名前の再公開の削除は、宣言の削除と重ねない
    let api = api_changes(
        &[
            ("src/lib.rs", "mod value;\n\npub use value::Amount;\n"),
            ("src/value.rs", "pub struct Amount;\n"),
        ],
        &[("src/lib.rs", "mod value;\n"), ("src/value.rs", "\n")],
    );
    let removals = api
        .removed
        .iter()
        .chain(&api.removed_dead)
        .filter(|symbol| symbol.name == "Amount")
        .count();
    assert_eq!(removals, 1, "{api:?}");
    let api = api_changes(
        &[("src/lib.rs", "mod value;\n"), ("src/value.rs", "\n")],
        &[
            ("src/lib.rs", lib),
            ("src/value.rs", "pub struct Amount;\n"),
        ],
    );
    assert_eq!(classify(&api, "Amount").0, "add", "{api:?}");
    assert_eq!(classify(&api, "Renamed"), ("none", None), "{api:?}");
    // 宣言が残れば、別名の削除は報告する
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/value.rs", "pub struct Amount;\n"),
        ],
        &[("src/lib.rs", "mod value;\n")],
    );
    assert!(
        matches!(classify(&api, "Renamed").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// edition 2015 の、先頭が識別子の経路は crate root から読む。
#[test]
fn edition_2015_bare_paths_resolve_from_the_crate_root() {
    let manifest = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2015\"\n";
    let api = api_changes_with(
        manifest,
        &[
            ("src/lib.rs", "mod detail;\npub mod api;\n"),
            DETAIL,
            ("src/api.rs", "pub use detail::make;\n"),
        ],
        &[("src/api.rs", "\n")],
    );
    assert!(
        matches!(classify(&api, "make"), ("rm" | "rm_dead", None)),
        "{api:?}"
    );
}

/// 非公開の外部の取り込みは、値・マクロの名前空間を決められなくても公開の名前を作らない。
#[test]
fn private_external_imports_are_not_public_names() {
    let api = api_changes(
        &[("src/lib.rs", "pub fn keep() {}\n")],
        &[("src/lib.rs", "pub fn keep() {}\nuse std::fmt::Debug;\n")],
    );
    assert_eq!(classify(&api, "Debug"), ("none", None), "{api:?}");
}

/// `#[path]` だけを変えても、その module を glob で取り込む名前は入れ替わる。
#[test]
fn path_changes_reach_the_globs_of_the_module() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[path = \"one.rs\"]\nmod selected;\npub use selected::*;\n",
            ),
            ("src/one.rs", "pub fn make() {}\n"),
            ("src/two.rs", "pub fn other() {}\n"),
        ],
        &[(
            "src/lib.rs",
            "#[path = \"two.rs\"]\nmod selected;\npub use selected::*;\n",
        )],
    );
    assert!(
        matches!(classify(&api, "make").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    assert_eq!(classify(&api, "other").0, "add", "{api:?}");
}

/// 末尾の `::self` の取り込みは型の名前空間だけ (`a::item::self` = `a::item::{self}`)。
#[test]
fn trailing_self_imports_only_the_type_namespace() {
    let modules = "mod a {\n    pub mod item {}\n    pub fn item() {}\n}\n";
    let old = format!("{modules}pub use a::item;\n");
    let new = format!("{modules}pub use a::item::self;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(
        matches!(classify(&api, "item").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// 項目の位置のマクロを足すと、字面にある名前が glob の名前を隠し得る (決められない)。
#[test]
fn macro_calls_that_may_hide_glob_names_are_unverified() {
    let old = "mod detail {\n    pub fn make() {}\n}\n\nmacro_rules! hide {\n    () => {\n        fn make() {}\n    };\n}\n\npub use detail::*;\n";
    let new = format!("{old}hide!();\n");
    let api = api_changes(&[("src/lib.rs", old)], &[("src/lib.rs", new.as_str())]);
    assert!(unverified(&api, "make"), "{api:?}");
}

/// マクロが字面に module の名前を持てば、その module を公開し得る (届かないと決めない)。
#[test]
fn modules_that_macros_may_expose_are_not_unreachable() {
    let tail = "macro_rules! expose {\n    () => {\n        pub use crate::hidden::api;\n    };\n}\nexpose!();\n";
    let old = format!(
        "mod detail {{\n    pub fn make() {{}}\n}}\n\nmod hidden {{\n    pub mod api {{\n        pub use crate::detail::make as release;\n    }}\n}}\n\n{tail}"
    );
    let new = format!(
        "mod detail {{\n    pub fn make() {{}}\n}}\n\nmod hidden {{\n    pub mod api {{}}\n}}\n\n{tail}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_ne!(classify(&api, "release"), ("none", None), "{api:?}");
}

/// edition だけを変えても、名前で始まる経路の読み方が変われば比べる。
#[test]
fn edition_changes_are_compared() {
    let manifest = |edition: &str| {
        format!("[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n")
    };
    let (old, new) = (manifest("2015"), manifest("2018"));
    let api = api_changes_with(
        &old,
        &[(
            "src/lib.rs",
            "pub fn item() {}\n\npub mod api {\n    pub struct item {}\n    pub use item as Exposed;\n}\n",
        )],
        &[("Cargo.toml", new.as_str())],
    );
    assert_ne!(classify(&api, "Exposed"), ("none", None), "{api:?}");
}

/// `cfg_attr` で export するマクロの再公開は、公開かを決められない。
#[test]
fn conditionally_exported_macros_are_unverified() {
    let definition =
        "#[cfg_attr(feature = \"x\", macro_export)]\nmacro_rules! m {\n    () => {};\n}\n";
    let old = format!("{definition}\n#[cfg(feature = \"x\")]\npub use crate::m as exposed;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", definition)],
    );
    assert!(unverified(&api, "exposed"), "{api:?}");
}

/// 列挙できない持ち込みは、経路全体の条件と見える範囲で外へ出るかを決める。
#[test]
fn unlisted_names_follow_the_conditions_and_visibility_of_the_whole_route() {
    let inner = "mod bridge {\n    #[cfg(feature = \"x\")]\n    pub use std::collections::*;\n}\n";
    let old = format!("{inner}\n#[cfg(not(feature = \"x\"))]\npub use bridge::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", inner)]);
    assert!(!incomplete(&api), "{api:?}");
    let inner = "mod bridge {\n    pub(crate) use std::collections::*;\n}\n";
    let old = format!("{inner}\npub use bridge::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", inner)]);
    assert!(!incomplete(&api), "{api:?}");
    // 外へ出る持ち込みの削除は、探し切れなかったことにする
    let inner = "mod bridge {\n    pub use std::collections::*;\n}\n";
    let old = format!("{inner}\npub use bridge::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", inner)]);
    assert!(incomplete(&api), "{api:?}");
}

/// `#[non_exhaustive]` の構造体のコンストラクタは、crate の外から使えない。
#[test]
fn non_exhaustive_constructors_are_not_public() {
    let base =
        "mod detail {\n    #[non_exhaustive]\n    pub struct S;\n}\n\npub type S = detail::S;\n";
    let old = format!("{base}pub use detail::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", base)]);
    assert_eq!(classify(&api, "S"), ("none", None), "{api:?}");
}

/// 同じ module の中の別名の長い連鎖も解く。
#[test]
fn long_alias_chains_in_one_module_are_resolved() {
    let base = "mod detail {\n    pub fn make() {}\n}\n\nuse crate::detail as a;\nuse a as b;\nuse b as c;\nuse c as d;\n\n";
    let old = format!("{base}pub use detail::make as exposed;\n");
    let new = format!("{base}pub use d::make as exposed;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "exposed"), ("none", None), "{api:?}");
}

/// 名前を持ち込まない glob の条件は数えない (原子が増えすぎて比べられなくならない)。
#[test]
fn unrelated_glob_conditions_are_not_counted() {
    let mut old = String::from("mod empty {}\n\n");
    for index in 0..17 {
        old.push_str(&format!(
            "#[cfg(feature = \"f{index}\")]\nuse crate::empty::*;\n"
        ));
    }
    let new = format!("{old}fn local() {{}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "local"), ("none", None), "{api:?}");
}

/// 非公開の外部 glob が自分の経路の先頭を解く循環でも、非公開の名前は公開の名前にしない。
#[test]
fn private_external_globs_do_not_make_public_names() {
    let api = api_changes(
        &[("src/lib.rs", "use std::collections::*;\n")],
        &[("src/lib.rs", "use std::collections::*;\nfn local() {}\n")],
    );
    assert_eq!(classify(&api, "local"), ("none", None), "{api:?}");
    let base = "mod detail {\n    pub(crate) fn helper() {}\n}\nuse ::std::collections::*;\n";
    let new = format!("{base}use detail::*;\n");
    let api = api_changes(&[("src/lib.rs", base)], &[("src/lib.rs", new.as_str())]);
    assert_eq!(classify(&api, "helper"), ("none", None), "{api:?}");
}

/// crate root の別名の再公開 (`pub use crate as api;`) の削除も比べる。
#[test]
fn crate_root_aliases_are_compared() {
    let api = api_changes(
        &[("src/lib.rs", "pub fn make() {}\n\npub use crate as api;\n")],
        &[("src/lib.rs", "pub fn make() {}\n")],
    );
    assert!(
        matches!(classify(&api, "api").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub fn make() {}\n\npub use crate::{self as api};\n",
        )],
        &[("src/lib.rs", "pub fn make() {}\n")],
    );
    assert!(
        matches!(classify(&api, "api").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// マクロの本体の字面の変化 (名前の集合が同じでも) と、入れ子のマクロの本体の変化も比べる。
#[test]
fn macro_body_changes_reach_their_calls() {
    let base = "mod detail {\n    pub fn make() {}\n}\npub use detail::*;\n\n";
    let old = format!(
        "{base}macro_rules! wrap {{\n    () => {{ const _: () = {{ fn make() {{}} }}; }};\n}}\nwrap!();\n"
    );
    let new = format!(
        "{base}macro_rules! wrap {{\n    () => {{ const _: () = {{}}; fn make() {{}} }};\n}}\nwrap!();\n"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "make"), "{api:?}");
    let outer = "macro_rules! outer {\n    () => { inner!(); };\n}\nouter!();\n";
    let old = format!("{base}macro_rules! inner {{\n    () => {{ fn other() {{}} }};\n}}\n{outer}");
    let new = format!("{base}macro_rules! inner {{\n    () => {{ fn make() {{}} }};\n}}\n{outer}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "make"), "{api:?}");
}

/// 呼び出すマクロの名前は、マクロが作る名前に数えない (何も作らないマクロの呼び出しでは変わらない)。
#[test]
fn macro_names_are_not_names_they_make() {
    let base = "mod detail {\n    pub fn make() {}\n}\npub use detail::*;\n\nmacro_rules! make {\n    () => {};\n}\n";
    let new = format!("{base}make!();\n");
    let api = api_changes(&[("src/lib.rs", base)], &[("src/lib.rs", new.as_str())]);
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// `cfg_attr` で付く `non_exhaustive` は、コンストラクタの公開を決めない。
#[test]
fn conditional_non_exhaustive_constructors_are_unverified() {
    let base = "mod detail {\n    #[cfg_attr(any(), non_exhaustive)]\n    pub struct S;\n}\n\npub type S = detail::S;\n";
    let old = format!("{base}pub use detail::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", base)]);
    assert_ne!(classify(&api, "S"), ("none", None), "{api:?}");
}

/// workspace から継承する edition の変化も、member の crate で比べる。
#[test]
fn inherited_edition_changes_are_compared() {
    let workspace = |edition: &str| {
        format!(
            "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n\n[workspace.package]\nedition = \"{edition}\"\n"
        )
    };
    let (old, new) = (workspace("2015"), workspace("2018"));
    let api = api_changes_with(
        &old,
        &[
            (
                "member/Cargo.toml",
                "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
            ),
            (
                "member/src/lib.rs",
                "pub fn item() {}\n\npub mod api {\n    pub struct item {}\n    pub use item as Exposed;\n}\n",
            ),
        ],
        &[("Cargo.toml", new.as_str())],
    );
    assert_ne!(classify(&api, "Exposed"), ("none", None), "{api:?}");
    // member を glob (`member*`) で書いても
    let globbed = |edition: &str| {
        format!(
            "[workspace]\nmembers = [\"member*\"]\nresolver = \"2\"\n\n[workspace.package]\nedition = \"{edition}\"\n"
        )
    };
    let (old, new) = (globbed("2015"), globbed("2018"));
    let api = api_changes_with(
        &old,
        &[
            (
                "member/Cargo.toml",
                "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
            ),
            (
                "member/src/lib.rs",
                "pub fn item() {}\n\npub mod api {\n    pub struct item {}\n    pub use item as Exposed;\n}\n",
            ),
        ],
        &[("Cargo.toml", new.as_str())],
    );
    assert_ne!(classify(&api, "Exposed"), ("none", None), "{api:?}");
}

/// 空の module の別名の glob の条件は、名前を持ち込まないので数えない。
#[test]
fn globs_of_empty_module_aliases_are_not_counted() {
    let mut old = String::from("mod empty {}\nuse crate::empty as alias;\n\n");
    for index in 0..17 {
        old.push_str(&format!(
            "#[cfg(feature = \"f{index}\")]\npub use alias::*;\n"
        ));
    }
    let new = format!("{old}fn local() {{}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "local"), ("none", None), "{api:?}");
}

/// 置き場だけを変えた module の非公開の名前は比べない。外部の項目の同じ再公開を、inline module から
/// ファイルへ移しても変化にしない。
#[test]
fn moved_placements_keep_private_and_unchanged_names() {
    let api = api_changes(
        &[
            ("src/lib.rs", "#[path = \"one.rs\"]\nmod internal;\n"),
            ("src/one.rs", "fn f0() {}\nfn f1() {}\n"),
            ("src/two.rs", "fn f0() {}\nfn f1() {}\n"),
        ],
        &[("src/lib.rs", "#[path = \"two.rs\"]\nmod internal;\n")],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(classify(&api, "f0"), ("none", None), "{api:?}");
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub mod api {\n    pub use ::std::fmt::Debug;\n}\n",
        )],
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", "pub use ::std::fmt::Debug;\n"),
        ],
    );
    assert_eq!(classify(&api, "Debug"), ("none", None), "{api:?}");
}

/// 列挙できない名前を外へ出す module が変われば、別の公開の module から出ることでは相殺しない。出す module が
/// 同じなら申告しない。
#[test]
fn holes_moved_between_public_modules_are_incomplete() {
    let hidden = "mod hidden {\n    pub use ::std::fmt::*;\n}\n\n";
    let old = format!("{hidden}pub mod a {{\n    pub use crate::hidden::*;\n}}\npub mod b {{}}\n");
    let new = format!("{hidden}pub mod a {{}}\npub mod b {{\n    pub use crate::hidden::*;\n}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(incomplete(&api), "{api:?}");
    let same = format!("{old}\npub fn local() {{}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", same.as_str())],
    );
    assert!(!incomplete(&api), "{api:?}");
}

/// 非公開の取り込みを、経路の途中の crate root の別名越しに使えば、門を開いて比べる。
#[test]
fn private_imports_used_through_root_aliases_are_compared() {
    let base = "mod a {\n    pub fn item() {}\n}\nmod b {\n    #[allow(non_camel_case_types)]\n    pub struct item {}\n}\n\n";
    let tail = "use crate as root_alias;\n\npub use root_alias::selected::item;\n";
    let old = format!("{base}use crate::a as selected;\n{tail}");
    let new = format!("{base}use crate::b as selected;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_ne!(classify(&api, "item"), ("none", None), "{api:?}");
}

/// 宣言だけの追加でも、`use` の経路の先頭の外部 crate の名前を隠せば比べる。
#[test]
fn declarations_hiding_extern_crates_are_compared() {
    let old = "pub use std::fmt as api;\n";
    let new = format!("{old}\n#[allow(non_camel_case_types)]\npub enum std {{\n    fmt,\n}}\n");
    let api = api_changes(&[("src/lib.rs", old)], &[("src/lib.rs", new.as_str())]);
    assert!(
        unverified(&api, "api") || classify(&api, "api") != ("none", None),
        "{api:?}"
    );
    // manifest の依存の名前の宣言も (`-` は `_`)
    let manifest = format!("{CARGO}\n[dependencies]\nsome-dep = \"1\"\n");
    let old = "pub use some_dep::Error as E;\n";
    let new = format!("{old}\npub mod some_dep {{\n    pub struct Error;\n}}\n");
    let api = api_changes_with(
        &manifest,
        &[("src/lib.rs", old)],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "E"), "{api:?}");
}

/// ファイルの module の `#[macro_export]` のマクロは、その module の置き場の条件で有る。
#[test]
fn exported_macros_follow_their_module_placement() {
    let defs = (
        "src/defs.rs",
        "#[macro_export]\nmacro_rules! m {\n    () => {};\n}\n",
    );
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub fn m() {}\n\nmod defs;\n\npub use crate::m as exposed;\n",
            ),
            defs,
        ],
        &[(
            "src/lib.rs",
            "pub fn m() {}\n\n#[cfg(feature = \"x\")]\nmod defs;\n\npub use crate::m as exposed;\n",
        )],
    );
    assert!(
        matches!(classify(&api, "exposed").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// 置き場の変わった module の非公開の名前も、子の module の取り込みが経路に使えば比べる。
#[test]
fn private_names_of_moved_modules_used_by_children_are_compared() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod a {\n    pub fn item() {}\n}\nmod b {\n    #[allow(non_camel_case_types)]\n    pub struct item {}\n}\n\n#[path = \"one.rs\"]\npub mod parent;\n",
            ),
            (
                "src/one.rs",
                "use crate::a as selected;\n#[path = \"child.rs\"]\npub mod child;\n",
            ),
            (
                "src/two.rs",
                "use crate::b as selected;\n#[path = \"child.rs\"]\npub mod child;\n",
            ),
            ("src/child.rs", "pub use super::selected::item;\n"),
        ],
        &[(
            "src/lib.rs",
            "mod a {\n    pub fn item() {}\n}\nmod b {\n    #[allow(non_camel_case_types)]\n    pub struct item {}\n}\n\n#[path = \"two.rs\"]\npub mod parent;\n",
        )],
    );
    assert_ne!(classify(&api, "item"), ("none", None), "{api:?}");
}

/// glob は自分の経路の先頭の名前を型の名前空間にだけ持ち込まない (同じ名前の関数は持ち込む)。
#[test]
fn glob_heads_are_skipped_only_in_the_type_namespace() {
    let base = "mod foo {\n    pub fn foo() {}\n}\n";
    let old = format!("{base}pub use foo::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", base)]);
    assert!(
        matches!(classify(&api, "foo").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// `super::super as api` (crate root に着く) と `super as parent` (crate root の外) の別名の削除も比べる。
#[test]
fn nested_super_aliases_are_compared() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub fn make() {}\n\npub mod a {\n    pub mod b {\n        pub use super::super as api;\n    }\n}\n",
        )],
        &[(
            "src/lib.rs",
            "pub fn make() {}\n\npub mod a {\n    pub mod b {}\n}\n",
        )],
    );
    assert!(
        matches!(classify(&api, "api").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub mod a {\n    pub fn f() {}\n    pub mod b {\n        pub use super as parent;\n    }\n}\n",
        )],
        &[(
            "src/lib.rs",
            "pub mod a {\n    pub fn f() {}\n    pub mod b {}\n}\n",
        )],
    );
    assert!(
        matches!(classify(&api, "parent").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// マクロを取り込みの別名で呼んでも、本体の変化を呼び出しへ伝える。
#[test]
fn macros_called_through_aliases_are_compared() {
    let base = "mod detail {\n    pub fn make() {}\n}\npub use detail::*;\n\n";
    let call = "use inner as alias;\nalias!();\n";
    let old =
        format!("{base}macro_rules! inner {{\n    () => {{ fn other() {{}} }};\n}}\n\n{call}");
    let new = format!("{base}macro_rules! inner {{\n    () => {{ fn make() {{}} }};\n}}\n\n{call}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "make"), "{api:?}");
}

/// 外部の module の別名の先を変えれば、同じ字面の再公開でも外部の項目は違う。
#[test]
fn foreign_items_through_changed_aliases_are_compared() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "use ::std::fmt as source;\n\npub use source::Error;\n",
        )],
        &[(
            "src/lib.rs",
            "use ::std::io as source;\n\npub use source::Error;\n",
        )],
    );
    assert!(unverified(&api, "Error"), "{api:?}");
    // 外部の module の再公開の先を変えれば、その下の名前を比べ切れない
    let api = api_changes(
        &[("src/lib.rs", "pub use ::std::fmt as api;\n")],
        &[("src/lib.rs", "pub use ::std::io as api;\n")],
    );
    assert!(unverified(&api, "api"), "{api:?}");
    // 同じ外部の module なら、書き方を変えても同じ
    let api = api_changes(
        &[("src/lib.rs", "pub use ::std::fmt as api;\n")],
        &[("src/lib.rs", "pub use std::fmt as api;\n")],
    );
    assert_eq!(classify(&api, "api"), ("none", None), "{api:?}");
}

/// 外部の項目を glob で取り込み直しても、同じ項目なら旧新で同じ (inline module をファイルへ移しても)。
#[test]
fn foreign_items_through_globs_are_the_same() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "pub mod api {\n    pub use ::std::fmt::Debug;\n}\npub use api::*;\n",
        )],
        &[
            ("src/lib.rs", "pub mod api;\npub use api::*;\n"),
            ("src/api.rs", "pub use ::std::fmt::Debug;\n"),
        ],
    );
    assert_eq!(classify(&api, "Debug"), ("none", None), "{api:?}");
}

/// `#[non_exhaustive]` の variant のコンストラクタは crate の外から使えないので、その再公開の削除は
/// 公開の名前の削除ではない。
#[test]
fn non_exhaustive_variant_constructors_are_not_public() {
    let base = "pub enum E {\n    #[non_exhaustive]\n    V(u8),\n    #[non_exhaustive]\n    W,\n}\n\npub type V = E;\npub type W = E;\n\nfn keep(_: V, _: W) {}\n";
    let old = format!("{base}\npub use E::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", base)]);
    assert_eq!(classify(&api, "V"), ("none", None), "{api:?}");
    assert_eq!(classify(&api, "W"), ("none", None), "{api:?}");
}

/// 同じスコープの別名の長い連なり (逆順) も解く。
#[test]
fn long_alias_chains_are_resolved() {
    let mut base = String::from("mod target {\n    pub fn item() {}\n}\n\n");
    let length = 300;
    for index in 0..length {
        base.push_str(&format!("use a{:04} as a{index:04};\n", index + 1));
    }
    base.push_str(&format!("use crate::target as a{length:04};\n\n"));
    let old = format!("{base}pub use a0000::item;\n");
    let new = format!("{base}#[cfg(feature = \"x\")]\npub use a0000::item;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    let (bucket, availability) = classify(&api, "item");
    assert!(matches!(bucket, "rm" | "rm_dead"), "{api:?}");
    assert_eq!(availability, cfg(&[&[]], &[&[X]]), "{api:?}");
}

/// マクロが作り得る名前 (module) を通る経路は、glob の循環の中でも中身の分からない先とする。
#[test]
fn macro_generated_modules_in_glob_cycles_are_incomplete() {
    let base = "mod a {\n    pub use crate::b::generated;\n}\n\nmod b {\n    pub use crate::a::*;\n\n    macro_rules! define {\n        () => {\n            pub mod generated {\n                pub fn item() {}\n            }\n        };\n    }\n    define!();\n}\n";
    let old = format!("{base}\npub use a::generated::*;\n");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", base)]);
    assert!(incomplete(&api), "{api:?}");
}

/// 移動していない別の enum への切り替えは、その下の variant を比べ切れない。
#[test]
fn switching_between_existing_enums_is_unverified() {
    let base = "mod a {\n    pub enum E {\n        Old,\n    }\n}\nmod b {\n    pub enum E {\n        New,\n    }\n}\n\n";
    let old = format!("{base}pub use a::E as Api;\n");
    let new = format!("{base}pub use b::E as Api;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "Api"), "{api:?}");
}

/// マクロの別名の指す先だけを変えても、その呼び出しが作る名前を比べる。
#[test]
fn changed_macro_aliases_reach_their_calls() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! first {\n    () => {};\n}\nmacro_rules! second {\n    () => { fn item() {} };\n}\n\n";
    let tail = "\npub mod api {\n    pub use crate::detail::*;\n    super::chosen!();\n}\n";
    let old = format!("{head}use first as chosen;\n{tail}");
    let new = format!("{head}use second as chosen;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `macro_rules!` の定義の条件を変えれば、呼ぶ本体が変わり得る。
#[test]
fn macro_definition_conditions_reach_their_calls() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let tail = "macro_rules! choose {\n    () => { fn item() {} };\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}#[cfg(feature = \"x\")]\n{tail}");
    let new = format!("{head}#[cfg(not(feature = \"x\"))]\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 型の取り込みの別名は、同じ名前のマクロの別名にしない。
#[test]
fn type_aliases_are_not_macro_aliases() {
    let head = "mod detail {\n    pub fn item() {}\n}\npub use detail::*;\n\nmod types {\n    #[allow(non_camel_case_types)]\n    pub struct inner {}\n}\nuse types::inner as alias;\n\nmacro_rules! alias {\n    () => {};\n}\nalias!();\n\n";
    let old = format!("{head}macro_rules! inner {{\n    () => {{ fn other() {{}} }};\n}}\n");
    let new = format!("{head}macro_rules! inner {{\n    () => {{ fn item() {{}} }};\n}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "item"), ("none", None), "{api:?}");
}

/// workspace の中の path 依存の package は暗黙の member で、継承する edition の変化を比べる。
#[test]
fn implicit_workspace_members_follow_inherited_editions() {
    let workspace = |edition: &str| {
        format!(
            "[workspace]\nmembers = [\"front\"]\nresolver = \"2\"\n\n[workspace.package]\nedition = \"{edition}\"\n"
        )
    };
    let (old, new) = (workspace("2015"), workspace("2018"));
    let api = api_changes_with(
        &old,
        &[
            (
                "front/Cargo.toml",
                "[package]\nname = \"front\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nimplicit = { path = \"../implicit\" }\n",
            ),
            ("front/src/lib.rs", ""),
            (
                "implicit/Cargo.toml",
                "[package]\nname = \"implicit\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
            ),
            (
                "implicit/src/lib.rs",
                "pub fn item() {}\n\npub mod api {\n    pub struct item {}\n    pub use item as Exposed;\n}\n",
            ),
        ],
        &[("Cargo.toml", new.as_str())],
    );
    assert_ne!(classify(&api, "Exposed"), ("none", None), "{api:?}");
}

/// enum と enum でないものの間の切り替えは、variant の名前が出入りするので比べ切れない。
#[test]
fn switching_between_enums_and_other_types_is_unverified() {
    let base =
        "mod a {\n    pub enum E {\n        V,\n    }\n}\nmod b {\n    pub struct S {}\n}\n\n";
    let old = format!("{base}pub use a::E as Api;\n");
    let new = format!("{base}pub use b::S as Api;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "Api"), "{api:?}");
}

/// glob で取り込み直したマクロを別名で呼んでも本体をたどり、型の取り込みを取り込み直した別名はマクロに
/// しない。
#[test]
fn macro_aliases_follow_the_macro_namespace() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod macros {\n";
    let tail = "    pub(crate) use inner;\n}\n\nmod bridge {\n    pub(crate) use crate::macros::*;\n}\n\nuse bridge::inner as chosen;\n\npub mod api {\n    pub use crate::detail::*;\n    super::chosen!();\n}\n";
    let old = format!("{head}    macro_rules! inner {{\n        () => {{}};\n    }}\n{tail}");
    let new = format!(
        "{head}    macro_rules! inner {{\n        () => {{ fn item() {{}} }};\n    }}\n{tail}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
    let head = "mod detail {\n    pub fn item() {}\n}\npub use detail::*;\n\nmod types {\n    #[allow(non_camel_case_types)]\n    pub struct inner {}\n}\nmod bridge {\n    pub(crate) use crate::types::inner;\n}\nuse bridge::inner as alias;\n\nmacro_rules! alias {\n    () => {};\n}\nalias!();\n\n";
    let old = format!("{head}macro_rules! inner {{\n    () => {{ fn other() {{}} }};\n}}\n");
    let new = format!("{head}macro_rules! inner {{\n    () => {{ fn item() {{}} }};\n}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "item"), ("none", None), "{api:?}");
}

/// 同じ名前のマクロの定義の並びを変えれば、呼ぶ本体が変わり得る。
#[test]
fn reordered_macro_definitions_reach_their_calls() {
    let detail =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let later = "macro_rules! choose {\n    () => { fn item() {} };\n}\n\n";
    let api_mod = "pub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n\n";
    let old = format!("{detail}{api_mod}{later}");
    let new = format!("{detail}{later}{api_mod}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `dep.workspace = true` で継承した path 依存も、workspace の暗黙の member にする。
#[test]
fn inherited_path_dependencies_are_workspace_members() {
    let workspace = |edition: &str| {
        format!(
            "[workspace]\nmembers = [\"front\"]\nresolver = \"2\"\n\n[workspace.package]\nedition = \"{edition}\"\n\n[workspace.dependencies]\nimplicit = {{ path = \"implicit\" }}\n"
        )
    };
    let (old, new) = (workspace("2015"), workspace("2018"));
    let api = api_changes_with(
        &old,
        &[
            (
                "front/Cargo.toml",
                "[package]\nname = \"front\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nimplicit.workspace = true\n",
            ),
            ("front/src/lib.rs", ""),
            (
                "implicit/Cargo.toml",
                "[package]\nname = \"implicit\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
            ),
            (
                "implicit/src/lib.rs",
                "pub fn item() {}\n\npub mod api {\n    pub struct item {}\n    pub use item as Exposed;\n}\n",
            ),
        ],
        &[("Cargo.toml", new.as_str())],
    );
    assert_ne!(classify(&api, "Exposed"), ("none", None), "{api:?}");
}

/// shebang の後のファイルの先頭の `#![cfg]` も、項目の有る条件に入る (#291)。
#[test]
fn file_conditions_after_a_shebang_are_compared() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "#!/usr/bin/env rust-script\n#![cfg(all())]\n\npub fn item() {}\n",
        )],
        &[(
            "src/lib.rs",
            "#!/usr/bin/env rust-script\n#![cfg(feature = \"x\")]\n\npub fn item() {}\n",
        )],
    );
    assert!(
        matches!(classify(&api, "item").0, "rm" | "rm_dead"),
        "{api:?}"
    );
}

/// 名前を変えた enum への切り替えは、variant が違えば比べ切れない。
#[test]
fn renamed_enums_are_not_moves() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "mod a {\n    pub enum E {\n        Old,\n    }\n}\nmod b {}\n\npub use a::E as Api;\n",
        )],
        &[(
            "src/lib.rs",
            "mod a {}\nmod b {\n    pub enum F {\n        New,\n    }\n}\n\npub use b::F as Api;\n",
        )],
    );
    assert!(unverified(&api, "Api"), "{api:?}");
}

/// 同じ名前のマクロの定義の本体を入れ替えれば、呼ぶ本体が変わり得る。
#[test]
fn swapped_macro_definitions_reach_their_calls() {
    let empty = "macro_rules! choose {\n    () => {};\n}\n";
    let making = "macro_rules! choose {\n    () => { fn item() {} };\n}\n";
    let call = "\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let detail = "mod detail {\n    pub fn item() {}\n}\n\n";
    let old = format!("{detail}{making}{empty}{call}");
    let new = format!("{detail}{empty}{making}{call}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// マクロの別名の先を、別の module の同じ名前のマクロへ変えても、呼ぶ本体が変わり得る。
#[test]
fn macro_aliases_to_same_named_macros_elsewhere_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\n";
    let tail = "\npub mod api {\n    pub use crate::detail::*;\n    super::chosen!();\n}\n";
    let old = format!("{head}use a::inner as chosen;\n{tail}");
    let new = format!("{head}use b::inner as chosen;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// enum から enum への切り替えは、variant が同じなら下の名前も同じで、違えば比べ切れない (inline module の
/// 間の移動は宣言の報告で組にならない)。
#[test]
fn switching_enums_compares_their_variants() {
    let api = api_changes(
        &[(
            "src/lib.rs",
            "mod a {\n    pub enum E {\n        Old,\n    }\n}\nmod b {}\n\npub use a::E as Api;\n",
        )],
        &[(
            "src/lib.rs",
            "mod a {}\nmod b {\n    pub enum E {\n        New,\n    }\n}\n\npub use b::E as Api;\n",
        )],
    );
    assert!(unverified(&api, "Api"), "{api:?}");
    let api = api_changes(
        &[(
            "src/lib.rs",
            "mod a {\n    pub enum E {\n        Same,\n    }\n}\nmod b {}\n\npub use a::E as Api;\n",
        )],
        &[(
            "src/lib.rs",
            "mod a {}\nmod b {\n    pub enum F {\n        Same,\n    }\n}\n\npub use b::F as Api;\n",
        )],
    );
    assert!(!unverified(&api, "Api"), "{api:?}");
}

/// 同じ名前のマクロの呼び出しの引数を入れ替えれば、作る名前が変わり得る。
#[test]
fn swapped_macro_calls_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\npub use detail::*;\n\nmacro_rules! choose {\n    ($name:ident) => {};\n}\n";
    let second = "macro_rules! choose {\n    ($name:ident) => { fn $name() {} };\n}\n";
    let old = format!("{head}choose!(item);\n\n{second}choose!(other);\n");
    let new = format!("{head}choose!(other);\n\n{second}choose!(item);\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 別名の先を取り込む module どうしで入れ替えても、呼ぶ本体が変わり得る。
#[test]
fn swapped_macro_alias_targets_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\n";
    let tail = "\npub mod api {\n    pub use crate::detail::*;\n    super::chosen!();\n}\n";
    let old = format!(
        "{head}use a::inner as chosen;\n\nmod unused {{\n    use crate::b::inner as chosen;\n}}\n{tail}"
    );
    let new = format!(
        "{head}use b::inner as chosen;\n\nmod unused {{\n    use crate::a::inner as chosen;\n}}\n{tail}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 型の名前空間だけの取り込み (`self as alias`) は、マクロの別名にしない。
#[test]
fn self_imports_are_not_macro_aliases() {
    let head = "mod detail {\n    pub fn item() {}\n}\npub use detail::*;\n\nmod inner {}\nuse crate::inner::{self as alias};\n\nmacro_rules! alias {\n    () => {};\n}\nalias!();\n\n";
    let old = format!("{head}macro_rules! inner {{\n    () => {{ fn other() {{}} }};\n}}\n");
    let new = format!("{head}macro_rules! inner {{\n    () => {{ fn item() {{}} }};\n}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "item"), ("none", None), "{api:?}");
}

/// enum の variant は、置き場 (cfg で分けたファイル) の条件ごとに比べる。
#[test]
fn enum_variants_are_compared_per_placement() {
    let modules = "#[cfg(feature = \"x\")]\n#[path = \"a_x.rs\"]\nmod a;\n#[cfg(not(feature = \"x\"))]\n#[path = \"a_other.rs\"]\nmod a;\n\n#[cfg(feature = \"x\")]\n#[path = \"b_x.rs\"]\nmod b;\n#[cfg(not(feature = \"x\"))]\n#[path = \"b_other.rs\"]\nmod b;\n\n";
    let old = format!("{modules}pub use a::E as Api;\n");
    let new = format!("{modules}pub use b::E as Api;\n");
    let api = api_changes(
        &[
            ("src/lib.rs", old.as_str()),
            ("src/a_x.rs", "pub enum E {\n    Old,\n}\n"),
            ("src/a_other.rs", "pub enum E {\n    New,\n}\n"),
            ("src/b_x.rs", "pub enum E {\n    New,\n}\n"),
            ("src/b_other.rs", "pub enum E {\n    Old,\n}\n"),
        ],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "Api"), "{api:?}");
}

/// マクロの別名の先を、取り込みの条件ごとに入れ替えても比べる。
#[test]
fn macro_alias_conditions_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\n";
    let tail = "\npub use detail::*;\nchosen!();\n";
    let old = format!(
        "{head}#[cfg(feature = \"x\")]\nuse a::inner as chosen;\n#[cfg(not(feature = \"x\"))]\nuse b::inner as chosen;\n{tail}"
    );
    let new = format!(
        "{head}#[cfg(feature = \"x\")]\nuse b::inner as chosen;\n#[cfg(not(feature = \"x\"))]\nuse a::inner as chosen;\n{tail}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// cfg で分けた同じ経路の inline module の、マクロの並びも比べる (別の宣言で上書きしない)。
#[test]
fn macro_orders_of_split_inline_modules_are_kept() {
    let body = |first: &str, second: &str| {
        format!(
            "mod detail {{\n    pub fn item() {{}}\n}}\n\n#[cfg(not(feature = \"x\"))]\npub mod api {{}}\n\n#[cfg(feature = \"x\")]\npub mod api {{\n    pub use crate::detail::*;\n\n    macro_rules! choose {{\n        ($name:ident) => {{}};\n    }}\n    choose!({first});\n\n    macro_rules! choose {{\n        ($name:ident) => {{ fn $name() {{}} }};\n    }}\n    choose!({second});\n}}\n"
        )
    };
    let (old, new) = (body("item", "other"), body("other", "item"));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 呼び出しだけのスコープの並びが変わっても、呼ぶ本体が変わり得る。
#[test]
fn call_orders_without_definitions_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    (install) => {\n        macro_rules! choose {\n            (item) => { fn item() {} };\n        }\n    };\n    (item) => {};\n}\n\n";
    let old = format!(
        "{head}pub mod api {{\n    pub use crate::detail::*;\n    choose!(item);\n    choose!(install);\n}}\n"
    );
    let new = format!(
        "{head}pub mod api {{\n    pub use crate::detail::*;\n    choose!(install);\n    choose!(item);\n}}\n"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// マクロの別名の取り込みの経路にある module の別名を、条件ごとに入れ替えても比べる。
#[test]
fn macro_alias_paths_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\n";
    let tail = "\nuse source::inner as chosen;\npub use detail::*;\nchosen!();\n";
    let old = format!(
        "{head}#[cfg(feature = \"x\")]\nuse a as source;\n#[cfg(not(feature = \"x\"))]\nuse b as source;\n{tail}"
    );
    let new = format!(
        "{head}#[cfg(feature = \"x\")]\nuse b as source;\n#[cfg(not(feature = \"x\"))]\nuse a as source;\n{tail}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 別々の名前のマクロの呼び出しでも、並びが変われば (呼び出しがマクロを定義し得る) 比べる。
#[test]
fn orders_of_different_macro_calls_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\nmod installer {\n    macro_rules! install {\n        () => {\n            macro_rules! choose {\n                () => { fn item() {} };\n            }\n        };\n    }\n    pub(crate) use install;\n}\n\n";
    let old = format!(
        "{head}pub mod api {{\n    use crate::installer::install;\n    pub use crate::detail::*;\n    choose!();\n    install!();\n}}\n"
    );
    let new = format!(
        "{head}pub mod api {{\n    use crate::installer::install;\n    pub use crate::detail::*;\n    install!();\n    choose!();\n}}\n"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// cfg で分けた同じ名前の module の条件を、マクロの定義をはさんで入れ替えても比べる。
#[test]
fn conditions_of_split_modules_between_macro_definitions_are_compared() {
    let body = |first: &str, second: &str| {
        format!(
            "mod detail {{\n    pub fn item() {{}}\n}}\n\nmacro_rules! choose {{\n    () => {{}};\n}}\n\n#[cfg({first})]\npub mod api {{\n    pub use crate::detail::*;\n    choose!();\n}}\n\nmacro_rules! choose {{\n    () => {{ fn item() {{}} }};\n}}\n\n#[cfg({second})]\npub mod api {{\n    pub use crate::detail::*;\n    choose!();\n}}\n"
        )
    };
    let x = "feature = \"x\"";
    let not_x = "not(feature = \"x\")";
    let (old, new) = (body(x, not_x), body(not_x, x));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 条件の無い inline module とファイルの module の間で同じ形の enum に付け替えても、変化にしない。
#[test]
fn enums_in_unconditional_modules_compare_equal() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod a {\n    pub enum E {\n        V,\n    }\n}\nmod b;\n\npub use a::E as Api;\n",
            ),
            ("src/b.rs", "pub enum E {\n    V,\n}\n"),
        ],
        &[(
            "src/lib.rs",
            "mod a {\n    pub enum E {\n        V,\n    }\n}\nmod b;\n\npub use b::E as Api;\n",
        )],
    );
    assert!(!unverified(&api, "Api"), "{api:?}");
}

/// `#[macro_use]` の module と呼ぶ module の前後を入れ替えれば、呼び出しの見る定義が変わり得る。
#[test]
fn macro_use_modules_reordered_reach_their_calls() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let defs = "#[macro_use]\nmod defs;\n\n";
    let api = "pub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n\n";
    let old = format!("{head}{defs}{api}");
    let new = format!("{head}{api}{defs}");
    let api_changes = api_changes(
        &[
            ("src/lib.rs", old.as_str()),
            (
                "src/defs.rs",
                "macro_rules! choose {\n    () => { fn item() {} };\n}\n",
            ),
        ],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api_changes, "item"), "{api_changes:?}");
}

/// マクロの別名の取り込みの経路にある module の別名を、もう 1 段先で変えても比べる。
#[test]
fn macro_alias_paths_through_module_alias_chains_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\n";
    let tail =
        "use selected as source;\nuse source::inner as chosen;\n\npub use detail::*;\nchosen!();\n";
    let old = format!("{head}use a as selected;\n{tail}");
    let new = format!("{head}use b as selected;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `#[macro_use]` の module だけを持つ親で、呼ぶ module の前後を入れ替えても比べる。
#[test]
fn macro_use_parents_reordered_reach_their_calls() {
    let detail = "mod detail {\n    pub fn item() {}\n}\n\n";
    let defaults =
        "#[macro_use]\nmod defaults {\n    macro_rules! choose {\n        () => {};\n    }\n}\n\n";
    let overrides = "#[macro_use]\nmod overrides {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\n";
    let api_mod = "pub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n\n";
    let old = format!("{detail}{defaults}{api_mod}{overrides}");
    let new = format!("{detail}{defaults}{overrides}{api_mod}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `#[macro_use]` を足せば、後の呼び出しの見る定義が変わり得る。
#[test]
fn added_macro_use_reaches_later_calls() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\n";
    let api_mod = "pub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}{api_mod}");
    let new = format!("{head}#[macro_use]\n{definitions}{api_mod}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 同じ条件の inline module とファイルの module の間で同じ形の enum に付け替えても、変化にしない。
#[test]
fn enums_in_equally_conditioned_modules_compare_equal() {
    let head = "#[cfg(feature = \"x\")]\nmod a {\n    pub enum E {\n        V,\n    }\n}\n\n#[cfg(feature = \"x\")]\nmod b;\n\n#[cfg(feature = \"x\")]\n";
    let old = format!("{head}pub use a::E as Api;\n");
    let new = format!("{head}pub use b::E as Api;\n");
    let api = api_changes(
        &[
            ("src/lib.rs", old.as_str()),
            ("src/b.rs", "pub enum E {\n    V,\n}\n"),
        ],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(!unverified(&api, "Api"), "{api:?}");
}

/// マクロの別名のある crate では、型の名前空間の取り込み (`self as source`) や glob の先を変えても、呼び出しの
/// 見る定義が変わり得る。
#[test]
fn imports_behind_macro_aliases_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\n";
    let tail = "use source::inner as chosen;\n\npub use detail::*;\nchosen!();\n";
    let old = format!("{head}use a::{{self as source}};\n{tail}");
    let new = format!("{head}use b::{{self as source}};\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
    let tail = "use inner as chosen;\n\npub use detail::*;\nchosen!();\n";
    let old = format!("{head}use a::*;\n{tail}");
    let new = format!("{head}use b::*;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// マクロの別名の終端の再取り込みの先を変えても比べる。
#[test]
fn macro_alias_terminals_are_compared() {
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\n";
    let tail = "\nuse bridge::inner as chosen;\npub use detail::*;\nchosen!();\n";
    let old = format!("{head}mod bridge {{\n    pub(crate) use crate::a::inner;\n}}\n{tail}");
    let new = format!("{head}mod bridge {{\n    pub(crate) use crate::b::inner;\n}}\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 内側の `#![macro_use]` を足しても、後の呼び出しの見る定義が変わり得る。
#[test]
fn inner_macro_use_reaches_later_calls() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let api_mod = "\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!(
        "{head}mod definitions {{\n    macro_rules! choose {{\n        () => {{ fn item() {{}} }};\n    }}\n}}\n{api_mod}"
    );
    let new = format!(
        "{head}mod definitions {{\n    #![macro_use]\n    macro_rules! choose {{\n        () => {{ fn item() {{}} }};\n    }}\n}}\n{api_mod}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `cfg_attr` の文字列の中の `macro_use` は属性ではない。
#[test]
fn macro_use_in_cfg_attr_strings_is_not_an_attribute() {
    let tail = "mod helpers {}\n\npub use detail::*;\nignore!(item);\n";
    let head = "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! ignore {\n    ($name:ident) => {};\n}\n\n";
    let old = format!("{head}{tail}");
    let new = format!("{head}#[cfg_attr(feature = \"x\", doc = \"macro_use\")]\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(!unverified(&api, "item"), "{api:?}");
}

/// enum の variant は設定ごとに比べる (相補的な条件の同じ名前の variant は、条件の無い variant と同じ)。
#[test]
fn complementary_variants_compare_equal_per_configuration() {
    let base = "mod a {\n    pub enum E {\n        #[cfg(feature = \"x\")]\n        V,\n        #[cfg(not(feature = \"x\"))]\n        V,\n    }\n}\nmod b {\n    pub enum E {\n        V,\n    }\n}\n\n";
    let old = format!("{base}pub use a::E as Api;\n");
    let new = format!("{base}pub use b::E as Api;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(!unverified(&api, "Api"), "{api:?}");
}

/// 内側の `#![cfg_attr(.., macro_use)]` の条件を入れ替えても、後の呼び出しの見る定義が変わり得る。
#[test]
fn inner_macro_use_conditions_are_compared() {
    let body = |condition: &str| {
        format!(
            "mod detail {{\n    pub fn item() {{}}\n}}\n\nmacro_rules! choose {{\n    () => {{}};\n}}\n\nmod definitions {{\n    #![cfg_attr({condition}, macro_use)]\n    macro_rules! choose {{\n        () => {{ fn item() {{}} }};\n    }}\n}}\n\npub mod api {{\n    pub use crate::detail::*;\n    choose!();\n}}\n"
        )
    };
    let (old, new) = (body("not(feature = \"x\")"), body("feature = \"x\""));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// マクロを取り込み直す module の置き場 (`#[path]`) を変えても、マクロの別名の呼び出しを比べる。
#[test]
fn moved_macro_reimports_reach_alias_calls() {
    let lib = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\nmod routing;\nuse routing::source::inner as chosen;\n\npub use detail::*;\nchosen!();\n";
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            (
                "src/routing.rs",
                "#[path = \"a_use.rs\"]\npub(crate) mod source;\n",
            ),
            ("src/a_use.rs", "pub(crate) use crate::a::inner;\n"),
            ("src/b_use.rs", "pub(crate) use crate::b::inner;\n"),
        ],
        &[(
            "src/routing.rs",
            "#[path = \"b_use.rs\"]\npub(crate) mod source;\n",
        )],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `cfg_attr` の中のコメントは、属性の名前の読み方を変えない。
#[test]
fn comments_in_cfg_attr_are_skipped() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}");
    let new = format!("{head}#[cfg_attr(feature = \"x\", /* export */ macro_use)]\n{definitions}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// マクロの別名のある crate でも、呼び出しの見る定義に届かない取り込みの変化では、呼び出しを比べ直さない
/// (呼ぶ名前と経路の名前から閉じた入力だけを比べる)。
#[test]
fn unrelated_imports_in_macro_alias_crates_do_not_recheck_calls() {
    let head = "mod macros {\n    macro_rules! discard {\n        ($name:ident) => {};\n    }\n    pub(crate) use discard;\n}\nuse macros::discard as ignore;\n\n";
    let tail = "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nignore!(item);\n";
    let old = format!("{head}mod helpers {{\n    use std::fmt;\n}}\n{tail}");
    let new = format!("{head}mod helpers {{\n    pub(crate) use std::fmt;\n}}\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "item").0, "none", "{api:?}");
}

/// enum の再公開の追加は、variant の条件の原子が多くても追加のまま (variant は切り替えのときだけ比べる)。
#[test]
fn enum_reexports_with_many_variant_conditions_are_additions() {
    let features: Vec<String> = (0..17)
        .map(|index| format!("feature = \"f{index}\""))
        .collect();
    let base = format!(
        "mod detail {{\n    pub enum E {{\n        #[cfg(any({}))]\n        V,\n    }}\n}}\n",
        features.join(", ")
    );
    let new = format!("{base}\npub use detail::E as Api;\n");
    let api = api_changes(
        &[("src/lib.rs", base.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "Api").0, "add", "{api:?}");
}

/// マクロの別名のある crate では、glob の経路の module の別名を変えても、呼び出しの見る定義が変わり得る。
#[test]
fn glob_paths_in_macro_alias_crates_are_compared() {
    let head = "mod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner as chosen;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner as chosen;\n}\n\n";
    let tail = "use selected::*;\n\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nchosen!();\n";
    let old = format!("{head}use a as selected;\n{tail}");
    let new = format!("{head}use b as selected;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// マクロを取り込み直す module の、cfg で分けた置き場の条件を入れ替えても比べる。
#[test]
fn conditional_reimport_placements_reach_alias_calls() {
    let lib = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! inner {\n        () => {};\n    }\n    pub(crate) use inner;\n}\nmod b {\n    macro_rules! inner {\n        () => { fn item() {} };\n    }\n    pub(crate) use inner;\n}\n\nmod routing;\nuse routing::source::inner as chosen;\npub use detail::*;\nchosen!();\n";
    let routing = |first: &str, second: &str| {
        format!(
            "#[cfg({first})]\n#[path = \"a_use.rs\"]\npub(crate) mod source;\n\n#[cfg({second})]\n#[path = \"b_use.rs\"]\npub(crate) mod source;\n"
        )
    };
    let x = "feature = \"x\"";
    let not_x = "not(feature = \"x\")";
    let (old, new) = (routing(x, not_x), routing(not_x, x));
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/routing.rs", old.as_str()),
            ("src/a_use.rs", "pub(crate) use crate::a::inner;\n"),
            ("src/b_use.rs", "pub(crate) use crate::b::inner;\n"),
        ],
        &[("src/routing.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// raw 文字列の中の `"` や `//` は、属性の名前の読み方を変えない。
#[test]
fn raw_strings_in_cfg_attr_are_kept() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}");
    let new = format!(
        "{head}#[cfg_attr(all(), doc = r#\"<a href=\"//example.org\">docs</a>\"#, macro_use)]\n{definitions}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// enum の再公開の削除は、宣言自身の削除と同じ名前なら重ねない。
#[test]
fn removed_enum_reexports_do_not_repeat_declarations() {
    let api = api_changes(
        &[
            ("src/lib.rs", "mod value;\npub use value::E;\n"),
            ("src/value.rs", "pub enum E {\n    V,\n}\n"),
        ],
        &[("src/lib.rs", "mod value;\n"), ("src/value.rs", "")],
    );
    let removals = api
        .removed
        .iter()
        .chain(&api.removed_dead)
        .filter(|symbol| symbol.name == "E")
        .count();
    assert_eq!(removals, 1, "{api:?}");
}

/// raw 文字列の中の `"` が後ろの属性を飲み込まない (`r#"""#` の後の `macro_use` は効く)。
#[test]
fn raw_string_quotes_keep_later_attributes() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}");
    let new = format!("{head}#[cfg_attr(all(), doc = r#\"\"\"#, macro_use)]\n{definitions}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// raw 文字列や lifetime の後ろの `,` と名前を、属性と読まない (文書を足しただけ)。
#[test]
fn literals_in_cfg_attr_do_not_make_attributes() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}");
    for attribute in [
        "#[cfg_attr(all(), doc = r#\"\",macro_use,\"\"#)]",
        "#[cfg_attr(all(), doc = r\",macro_use,\")]",
        "#[cfg_attr(all(), doc = \"',macro_use,'\")]",
    ] {
        let new = format!("{head}{attribute}\n{definitions}");
        let api = api_changes(
            &[("src/lib.rs", old.as_str())],
            &[("src/lib.rs", new.as_str())],
        );
        assert_eq!(classify(&api, "item").0, "none", "{attribute}: {api:?}");
    }
}

/// lifetime の `'` は文字の始まりではない (後ろの `macro_use` は効く)。
#[test]
fn lifetimes_in_cfg_attr_keep_later_attributes() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}");
    let new = format!(
        "{head}#[cfg_attr(all(), tool::marker(T<'a>), macro_use, tool::marker(U<'b>))]\n{definitions}"
    );
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// cfg で分けた module の同じ名前の enum の再公開は、その設定で名前を束縛した宣言を指す (別の設定の
/// 宣言の追加で、再公開の追加を消さない)。
#[test]
fn conditional_enum_reexports_keep_their_declarations() {
    let modules = "#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod source;\n\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod source;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", modules),
            ("src/a.rs", ""),
            ("src/b.rs", "pub enum E {\n    B,\n}\n"),
        ],
        &[
            (
                "src/lib.rs",
                format!("{modules}\n#[cfg(feature = \"x\")]\npub use source::E;\n").as_str(),
            ),
            ("src/a.rs", "pub enum E {\n    A,\n}\n"),
        ],
    );
    let added: Vec<&str> = api
        .added
        .iter()
        .filter(|symbol| symbol.name == "E")
        .map(|symbol| symbol.file.as_str())
        .collect();
    assert!(added.contains(&"src/a.rs"), "{api:?}");
    assert!(added.contains(&"src/lib.rs"), "{api:?}");
}

/// cfg で分けた module の enum の再公開の削除は、その設定の宣言の削除と重ねない。
#[test]
fn conditional_enum_reexport_removals_do_not_repeat_declarations() {
    let modules = "#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod source;\n\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod source;\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                format!("{modules}\n#[cfg(feature = \"x\")]\npub use source::E;\n").as_str(),
            ),
            ("src/a.rs", "pub enum E {\n    A,\n}\n"),
            ("src/b.rs", "pub enum E {\n    B,\n}\n"),
        ],
        &[("src/lib.rs", modules), ("src/b.rs", "")],
    );
    let removals: Vec<&str> = api
        .removed
        .iter()
        .chain(&api.removed_dead)
        .filter(|symbol| symbol.name == "E")
        .map(|symbol| symbol.file.as_str())
        .collect();
    assert_eq!(removals, ["src/b.rs"], "{api:?}");
}

/// 同じ名前のまま取り込むマクロでも、定義を置いたファイルの着く条件を入れ替えれば、呼び出しが作り得る名前を
/// 比べる。
#[test]
fn same_name_macro_imports_follow_definition_placements() {
    let lib = "mod routing;\nuse routing::source::choose;\n\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nchoose!();\n";
    let routing = |first: &str, second: &str| {
        format!(
            "#[cfg({first})]\n#[path = \"a.rs\"]\npub(crate) mod source;\n\n#[cfg({second})]\n#[path = \"b.rs\"]\npub(crate) mod source;\n"
        )
    };
    let x = "feature = \"x\"";
    let not_x = "not(feature = \"x\")";
    let (old, new) = (routing(x, not_x), routing(not_x, x));
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/routing.rs", old.as_str()),
            (
                "src/a.rs",
                "macro_rules! choose {\n    () => {};\n}\npub(crate) use choose;\n",
            ),
            (
                "src/b.rs",
                "macro_rules! choose {\n    () => { fn item() {} };\n}\npub(crate) use choose;\n",
            ),
        ],
        &[("src/routing.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 同じ名前のまま取り込み直すマクロでも、取り込みを置いたファイルの着く条件を入れ替えれば比べる。
#[test]
fn same_name_macro_reimports_follow_placements() {
    let lib = "mod detail {\n    pub fn item() {}\n}\n\nmod a {\n    macro_rules! choose {\n        () => {};\n    }\n    pub(crate) use choose;\n}\nmod b {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n    pub(crate) use choose;\n}\n\nmod routing;\nuse routing::source::choose;\npub use detail::*;\nchoose!();\n";
    let routing = |first: &str, second: &str| {
        format!(
            "#[cfg({first})]\n#[path = \"a_use.rs\"]\npub(crate) mod source;\n\n#[cfg({second})]\n#[path = \"b_use.rs\"]\npub(crate) mod source;\n"
        )
    };
    let x = "feature = \"x\"";
    let not_x = "not(feature = \"x\")";
    let (old, new) = (routing(x, not_x), routing(not_x, x));
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/routing.rs", old.as_str()),
            ("src/a_use.rs", "pub(crate) use crate::a::choose;\n"),
            ("src/b_use.rs", "pub(crate) use crate::b::choose;\n"),
        ],
        &[("src/routing.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 属性の中のマクロの引数のように、識別子の後に空白を挟んで続く raw 文字列も、後ろの属性を飲み込まない。
#[test]
fn raw_strings_after_identifiers_keep_later_attributes() {
    let head = "macro_rules! docs {\n    ($lang:ident $text:literal) => { $text };\n}\nmacro_rules! choose {\n    () => {};\n}\nmod detail {\n    pub fn item() {}\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}");
    let new =
        format!("{head}#[cfg_attr(all(), doc = docs!(en r#\"\"\"#), macro_use)]\n{definitions}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 同じ module の同じ名前の関数を足しても、別の名前空間の enum の新しい再公開は覆わない。
#[test]
fn reexports_are_not_covered_by_other_namespaces() {
    let enum_item = "    pub enum E {\n        V,\n    }\n";
    let api = api_changes(
        &[(
            "src/lib.rs",
            format!("pub mod detail {{\n{enum_item}}}\n").as_str(),
        )],
        &[(
            "src/lib.rs",
            format!(
                "pub mod detail {{\n{enum_item}    #[allow(non_snake_case)]\n    pub fn E() {{}}\n}}\npub use detail::E as Api;\n"
            )
            .as_str(),
        )],
    );
    assert_eq!(classify(&api, "Api").0, "add", "{api:?}");
    assert_eq!(classify(&api, "E").0, "add", "{api:?}");
}

/// 属性の名前の raw identifier (`#[r#macro_use]`・`cfg_attr` の中の `r#macro_use`) も同じ属性。
#[test]
fn raw_identifier_attributes_are_read() {
    let head =
        "mod detail {\n    pub fn item() {}\n}\n\nmacro_rules! choose {\n    () => {};\n}\n\n";
    let definitions = "mod definitions {\n    macro_rules! choose {\n        () => { fn item() {} };\n    }\n}\n\npub mod api {\n    pub use crate::detail::*;\n    choose!();\n}\n";
    let old = format!("{head}{definitions}");
    for attribute in ["#[r#macro_use]", "#[cfg_attr(all(), r#macro_use)]"] {
        let new = format!("{head}{attribute}\n{definitions}");
        let api = api_changes(
            &[("src/lib.rs", old.as_str())],
            &[("src/lib.rs", new.as_str())],
        );
        assert!(unverified(&api, "item"), "{attribute}: {api:?}");
    }
}

/// 同じ名前のマクロを持つ 2 つの module (`a` は名前を作り、`b` は作らない)。
const MACRO_MODULES: &str = "mod a {\n    macro_rules! m {\n        ($n:ident) => { pub struct $n; };\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        ($n:ident) => {};\n    }\n    pub(crate) use m;\n}\n";

/// マクロを持ち込む glob の経路の束縛を変えれば、その glob から取り込むマクロの呼び出しを比べる。
#[test]
fn glob_path_bindings_reach_macro_calls() {
    let routing = |source: &str| {
        format!(
            "{MACRO_MODULES}mod routing {{\n    use crate::{source} as selected;\n    pub(crate) use selected::*;\n}}\nmod out {{\n    use crate::routing::m;\n    m!(Foo);\n}}\npub use out::*;\n"
        )
    };
    let (old, new) = (routing("a"), routing("b"));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "Foo"), "{api:?}");
}

/// 経路で呼ぶマクロ (`crate::selected::m!`) は、経路の名前の束縛を変えれば比べる。
#[test]
fn qualified_macro_call_paths_are_compared() {
    let source = |source: &str| {
        format!(
            "{MACRO_MODULES}use crate::{source} as selected;\nmod out {{\n    crate::selected::m!(Foo);\n}}\npub use out::*;\n"
        )
    };
    let (old, new) = (source("a"), source("b"));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "Foo"), "{api:?}");
}

/// `#[macro_use]` を外せば、同じ名前の定義が 1 つずつでも、取り込みより先に見ていた定義が変わる。
#[test]
fn removed_macro_use_reaches_aliased_calls() {
    let tail = "mod defs {\n    macro_rules! m {\n        ($n:ident) => { pub struct $n; };\n    }\n}\nmod fallback {\n    macro_rules! n {\n        ($n:ident) => {};\n    }\n    pub(crate) use n;\n}\nmod out {\n    use crate::fallback::n as m;\n    m!(Foo);\n}\npub use out::*;\n";
    let old = format!("#[macro_use]\n{tail}");
    let api = api_changes(&[("src/lib.rs", old.as_str())], &[("src/lib.rs", tail)]);
    assert!(unverified(&api, "Foo"), "{api:?}");
}

/// 経路で呼ぶマクロの経路の名前を束縛する非公開の取り込みを変えれば、比べる (呼び出しのある module にしか
/// glob が無くても)。
#[test]
fn qualified_macro_call_paths_open_the_comparison() {
    let head = "mod a {\n    macro_rules! m {\n        () => {};\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        () => { fn item() {} };\n    }\n    pub(crate) use m;\n}\nmod detail {\n    pub fn item() {}\n}\n\n";
    let tail = "\npub mod api {\n    pub use crate::detail::*;\n    crate::selected::m!();\n}\n";
    let old = format!("{head}use a as selected;\n{tail}");
    let new = format!("{head}use b as selected;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 呼び出しの経路の名前は、コメントを挟んでも同じ名前。
#[test]
fn macro_call_paths_ignore_comments() {
    let head = "mod a {\n    macro_rules! m {\n        () => {};\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        () => { fn item() {} };\n    }\n    pub(crate) use m;\n}\nmod detail {\n    pub fn item() {}\n}\n\n";
    let tail = "\npub use detail::*;\ncrate::selected /* routing */ ::m!();\n";
    let old = format!("{head}use a as selected;\n{tail}");
    let new = format!("{head}use b as selected;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `#[macro_use] extern crate` の crate を替えれば、取り込みの無い名前の呼び出しを比べる (`as _` でも)。
#[test]
fn macro_use_extern_crates_reach_calls() {
    let manifest = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep_a = { path = \"dep_a\" }\ndep_b = { path = \"dep_b\" }\n";
    let dependency = |name: &str, body: &str| {
        (
            format!("{name}/Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            format!("{name}/src/lib.rs"),
            format!("#[macro_export]\nmacro_rules! m {{\n    ($name:ident) => {{ {body} }};\n}}\n"),
        )
    };
    let a = dependency("dep_a", "");
    let b = dependency("dep_b", "fn $name() {}");
    let tail = "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nm!(item);\n";
    for alias in ["", " as _"] {
        let old = format!("#[macro_use]\nextern crate dep_a{alias};\n{tail}");
        let new = format!("#[macro_use]\nextern crate dep_b{alias};\n{tail}");
        let api = api_changes_with(
            manifest,
            &[
                ("src/lib.rs", old.as_str()),
                (a.0.as_str(), a.1.as_str()),
                (a.2.as_str(), a.3.as_str()),
                (b.0.as_str(), b.1.as_str()),
                (b.2.as_str(), b.3.as_str()),
            ],
            &[("src/lib.rs", new.as_str())],
        );
        assert!(unverified(&api, "item"), "{alias}: {api:?}");
    }
}

/// 型だけの取り込みの別名を変えても、同じ名前のマクロの呼び出しは比べ直さない (マクロの名前空間に入らない)。
#[test]
fn type_imports_do_not_change_macro_calls() {
    let head = "macro_rules! ignore {\n    ($name:ident) => {};\n}\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nignore!(item);\n\n";
    let helpers = |target: &str| {
        format!(
            "{head}mod helpers {{\n    struct A;\n    struct B;\n    use self::{target} as ignore;\n}}\n"
        )
    };
    let (old, new) = (helpers("A"), helpers("B"));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "item").0, "none", "{api:?}");
}

/// 同じマクロを多くの module から経路で呼んでも、呼び出しの入力は線形に求まる (上限で打ち切らない)。
#[test]
fn many_qualified_calls_share_their_inputs() {
    let shared = "mod defs {\n    macro_rules! m {\n        () => {};\n    }\n    pub(crate) use m;\n}\nmod detail {\n    pub fn f() {}\n}\n";
    let calls: String = (0..1500)
        .map(|i| format!("mod p{i} {{ use crate::defs::m; crate::p{i}::m!(); }}\n"))
        .collect();
    let old = format!("{shared}pub use detail::f as api;\n{calls}");
    let new = format!("{shared}pub use detail::f as renamed;\n{calls}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(!incomplete(&api), "{:?}", api.parse_truncations);
    assert_eq!(classify(&api, "renamed").0, "add", "{api:?}");
}

/// `#[macro_export]` のマクロも、定義した module の中では textual scope の名前として取り込める。
#[test]
fn exported_macros_imported_in_their_module_are_compared() {
    let api = |target: &str| {
        format!(
            "mod detail {{\n    pub fn item() {{}}\n}}\n\npub mod api {{\n    #[macro_export]\n    macro_rules! a {{\n        ($n:ident) => {{}};\n    }}\n    #[macro_export]\n    macro_rules! b {{\n        ($n:ident) => {{ fn $n() {{}} }};\n    }}\n\n    use {target} as selected;\n\n    pub use crate::detail::*;\n    selected!(item);\n}}\n"
        )
    };
    let (old, new) = (api("a"), api("b"));
    let changes = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&changes, "item"), "{changes:?}");
}

/// `#[macro_use(m)]` の取り込むマクロのリストを替えれば、呼び出しを比べる。
#[test]
fn macro_use_lists_reach_calls() {
    let manifest = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep_a = { path = \"dep_a\" }\ndep_b = { path = \"dep_b\" }\n";
    let package = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
    };
    let (manifest_a, manifest_b) = (package("dep_a"), package("dep_b"));
    let lib_a = "#[macro_export]\nmacro_rules! m {\n    ($n:ident) => {};\n}\n";
    let lib_b = "#[macro_export]\nmacro_rules! m {\n    ($n:ident) => { fn $n() {} };\n}\n#[macro_export]\nmacro_rules! spare {\n    () => {};\n}\n";
    let tail = "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nm!(item);\n";
    let old = format!(
        "#[macro_use(m)]\nextern crate dep_a;\n#[macro_use(spare)]\nextern crate dep_b;\n{tail}"
    );
    let new = format!(
        "#[macro_use(m)]\nextern crate dep_a;\n#[macro_use(m)]\nextern crate dep_b;\n{tail}"
    );
    let api = api_changes_with(
        manifest,
        &[
            ("src/lib.rs", old.as_str()),
            ("dep_a/Cargo.toml", manifest_a.as_str()),
            ("dep_a/src/lib.rs", lib_a),
            ("dep_b/Cargo.toml", manifest_b.as_str()),
            ("dep_b/src/lib.rs", lib_b),
        ],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 引数で受け取った名前をマクロとして呼ぶマクロ (`$callee!($n)`) は、その名前の束縛を変えれば比べる。
#[test]
fn macro_names_passed_as_arguments_are_compared() {
    let head = "mod a {\n    macro_rules! m {\n        ($n:ident) => {};\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        ($n:ident) => { fn $n() {} };\n    }\n    pub(crate) use m;\n}\nmacro_rules! invoke {\n    ($callee:ident, $n:ident) => { $callee!($n); };\n}\n\n";
    let tail =
        "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\ninvoke!(chosen, item);\n";
    let old = format!("{head}use a::m as chosen;\n{tail}");
    let new = format!("{head}use b::m as chosen;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 同じスコープに同じ名前のマクロがあっても、型の取り込みの変化ではマクロの呼び出しを比べ直さない
/// (マクロの名前空間に入り得るかは、取り込みごとに先で決める)。
#[test]
fn type_imports_beside_macros_do_not_change_calls() {
    let source = |target: &str| {
        format!(
            "macro_rules! ignore {{\n    ($name:ident) => {{}};\n}}\nstruct A;\nstruct B;\n\nuse {target} as ignore;\n\nmod detail {{\n    pub fn item() {{}}\n}}\npub use detail::*;\nignore!(item);\n"
        )
    };
    let (old, new) = (source("A"), source("B"));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "item").0, "none", "{api:?}");
}

/// 多くのマクロの定義を多くの module が glob で取り込んでも、マクロの名前空間の判定を数え上げない。
#[test]
fn many_globbed_macro_definitions_are_not_enumerated() {
    let definitions: String = (0..2000)
        .map(|i| format!("macro_rules! m{i} {{ () => {{}}; }}\n"))
        .collect();
    let importers: String = (0..2000)
        .map(|i| format!("mod p{i} {{ use crate::defs::*; }}\n"))
        .collect();
    let shared = format!(
        "mod defs {{\n{definitions}}}\nmod detail {{\n    pub fn f() {{}}\n}}\n{importers}"
    );
    let old = format!("{shared}pub use detail::f as api;\n");
    let new = format!("{shared}pub use detail::f as renamed;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(!incomplete(&api), "{:?}", api.parse_truncations);
    assert_eq!(classify(&api, "renamed").0, "add", "{api:?}");
}

/// export したマクロを同じスコープで取り込む `use m as x;` は、crate root のマクロと同じもの (公開のまま)。
#[test]
fn exported_macros_reimported_by_name_stay_public() {
    let head =
        "pub mod api {\n    #[macro_export]\n    macro_rules! m {\n        () => {};\n    }\n\n";
    let old = format!("{head}    pub use m as exported;\n}}\n");
    let new = format!("{head}    #[cfg(feature = \"x\")]\n    pub use m as exported;\n}}\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(
        matches!(classify(&api, "exported").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    let same = api_changes(
        &[(
            "src/lib.rs",
            format!("{head}    pub use crate::m;\n    pub use crate::m as exported;\n}}\n")
                .as_str(),
        )],
        &[(
            "src/lib.rs",
            format!("{head}    pub use crate::m;\n    pub use m as exported;\n}}\n").as_str(),
        )],
    );
    assert_eq!(classify(&same, "exported").0, "none", "{same:?}");
}

/// 本体が、引数をマクロの名前に使うマクロへ固定の名前を渡すなら、その名前の束縛も比べる。
#[test]
fn names_forwarded_to_dynamic_macros_are_compared() {
    let head = "mod a {\n    macro_rules! m {\n        ($n:ident) => {};\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        ($n:ident) => { fn $n() {} };\n    }\n    pub(crate) use m;\n}\nmacro_rules! invoke {\n    ($callee:ident, $n:ident) => { $callee!($n); };\n}\nmacro_rules! outer {\n    () => { invoke!(chosen, item); };\n}\n\n";
    let tail = "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nouter!();\n";
    let old = format!("{head}use a::m as chosen;\n{tail}");
    let new = format!("{head}use b::m as chosen;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 繰り返しでマクロの名前を組み立てる (`$($callee)+ !`) マクロも、引数の名前の束縛を比べる。
#[test]
fn repeated_macro_names_are_compared() {
    let head = "mod a {\n    macro_rules! m {\n        ($n:ident) => {};\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        ($n:ident) => { fn $n() {} };\n    }\n    pub(crate) use m;\n}\nmacro_rules! invoke {\n    (($($callee:tt)+), $n:ident) => { $($callee)+ !($n); };\n}\n\n";
    let tail =
        "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\ninvoke!((chosen), item);\n";
    let old = format!("{head}use a::m as chosen;\n{tail}");
    let new = format!("{head}use b::m as chosen;\n{tail}");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 外部 crate の glob が持ち込み得るマクロを取り込む別名も、マクロの名前空間に入り得る。
#[test]
fn macros_from_external_globs_are_compared() {
    let manifest = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep_a = { path = \"dep_a\" }\ndep_b = { path = \"dep_b\" }\n";
    let package = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
    };
    let (manifest_a, manifest_b) = (package("dep_a"), package("dep_b"));
    let lib_a = "#[macro_export]\nmacro_rules! m {\n    ($n:ident) => {};\n}\n";
    let lib_b = "#[macro_export]\nmacro_rules! m {\n    ($n:ident) => { fn $n() {} };\n}\n";
    let head =
        "mod a {\n    pub(crate) use dep_a::*;\n}\nmod b {\n    pub(crate) use dep_b::*;\n}\n\n";
    let tail = "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nselected!(item);\n";
    let old = format!("{head}use a::m as selected;\n{tail}");
    let new = format!("{head}use b::m as selected;\n{tail}");
    let api = api_changes_with(
        manifest,
        &[
            ("src/lib.rs", old.as_str()),
            ("dep_a/Cargo.toml", manifest_a.as_str()),
            ("dep_a/src/lib.rs", lib_a),
            ("dep_b/Cargo.toml", manifest_b.as_str()),
            ("dep_b/src/lib.rs", lib_b),
        ],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// `$crate::` は引数ではないので、引数の名前の型の取り込みの変化で呼び出しを比べ直さない。
#[test]
fn dollar_crate_paths_do_not_pass_arguments() {
    let head = "#[macro_export]\nmacro_rules! noop {\n    () => {};\n}\nmacro_rules! ignore {\n    ($arg:ident) => { $crate::noop!(); };\n}\n\n";
    let helpers = |target: &str| {
        format!(
            "{head}mod helpers {{\n    struct A {{}}\n    struct B {{}}\n    use {target} as tag;\n}}\n\nmod detail {{\n    pub fn tag() {{}}\n}}\npub use detail::*;\nignore!(tag);\n"
        )
    };
    let (old, new) = (helpers("A"), helpers("B"));
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "tag").0, "none", "{api:?}");
}

/// 呼び出しのマクロに届かない多くの取り込みは、glob の連なりが長くても、マクロの名前空間かを問わない。
#[test]
fn unrelated_imports_through_glob_chains_are_not_queried() {
    let n = 2000;
    let definitions: String = (0..n).map(|i| format!("pub fn f{i}() {{}}\n")).collect();
    let bridges: String = (0..n)
        .map(|i| {
            let next = if i + 1 < n {
                format!("g{}", i + 1)
            } else {
                "defs".to_owned()
            };
            format!("mod g{i} {{ pub(crate) use crate::{next}::*; }}\n")
        })
        .collect();
    let aliases = (0..n)
        .map(|i| format!("f{i} as a{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let shared = format!(
        "macro_rules! ignore {{ () => {{}}; }}\nmod defs {{\n{definitions}}}\n{bridges}mod query {{ use crate::g0::{{{aliases}}}; }}\nmod detail {{\n    pub fn leaf() {{}}\n}}\nignore!();\n"
    );
    let old = format!("{shared}pub use detail::leaf as api;\n");
    let new = format!("{shared}pub use detail::leaf as renamed;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(!incomplete(&api), "{:?}", api.parse_truncations);
    assert_eq!(classify(&api, "renamed").0, "add", "{api:?}");
}

/// export したマクロと同じ名前の module を `self` で取り込むなら、型の名前空間は書いたとおり (module の再公開)。
#[test]
fn self_imports_beside_exported_macros_keep_their_modules() {
    let head = "pub mod api {\n    #[macro_export]\n    macro_rules! m {\n        () => {};\n    }\n\n    pub mod m {\n        pub fn f() {}\n    }\n\n";
    let api = api_changes(
        &[(
            "src/lib.rs",
            format!("{head}    pub use m::{{self as exported}};\n}}\n").as_str(),
        )],
        &[(
            "src/lib.rs",
            format!(
                "{head}    #[cfg(feature = \"x\")]\n    pub use m::{{self as exported}};\n}}\n"
            )
            .as_str(),
        )],
    );
    assert!(
        matches!(classify(&api, "exported").0, "rm" | "rm_dead"),
        "{api:?}"
    );
    let same = api_changes(
        &[(
            "src/lib.rs",
            format!("{head}    pub use self::m as exported;\n}}\n").as_str(),
        )],
        &[(
            "src/lib.rs",
            format!("{head}    pub use m::{{self as exported}};\n}}\n").as_str(),
        )],
    );
    assert_eq!(classify(&same, "exported").0, "none", "{same:?}");
}

/// crate に定義の無いマクロ (外部 crate のもの) へ引数を渡すマクロや、その直接の呼び出しは、引数の名前のマクロの
/// 束縛を比べる。
#[test]
fn arguments_forwarded_to_external_macros_are_compared() {
    let manifest = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndispatcher = { path = \"dispatcher\" }\n";
    let dispatcher = "#[macro_export]\nmacro_rules! invoke {\n    ($callee:ident, $n:ident) => { $callee!($n); };\n}\n";
    let package = "[package]\nname = \"dispatcher\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    let head = "use dispatcher::invoke;\n\nmacro_rules! outer {\n    ($callee:ident, $n:ident) => {\n        $crate::invoke!($callee, $n);\n    };\n}\n\nmod a {\n    macro_rules! m {\n        ($n:ident) => {};\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        ($n:ident) => { fn $n() {} };\n    }\n    pub(crate) use m;\n}\n\n";
    for call in ["outer!(chosen, item);", "invoke!(chosen, item);"] {
        let tail =
            format!("\nmod detail {{\n    pub fn item() {{}}\n}}\npub use detail::*;\n{call}\n");
        let old = format!("{head}use a::m as chosen;\n{tail}");
        let new = format!("{head}use b::m as chosen;\n{tail}");
        let api = api_changes_with(
            manifest,
            &[
                ("src/lib.rs", old.as_str()),
                ("dispatcher/Cargo.toml", package),
                ("dispatcher/src/lib.rs", dispatcher),
            ],
            &[("src/lib.rs", new.as_str())],
        );
        assert!(unverified(&api, "item"), "{call}: {api:?}");
    }
}

/// 多くの定義が長い別名の連なりを呼んでも、引数を使い得るマクロの判定は別名の閉包を展開しない。
#[test]
fn long_alias_chains_do_not_expand_dynamic_callers() {
    let n = 3000;
    let mut shared = String::from(
        "macro_rules! target {\n    ($m:ident) => { $m!(); };\n}\nmacro_rules! ignore {\n    () => {};\n}\n",
    );
    shared.push_str(&format!("use target as a{};\n", n - 1));
    for i in (0..n - 1).rev() {
        shared.push_str(&format!("use a{} as a{i};\n", i + 1));
    }
    for i in 0..n {
        shared.push_str(&format!(
            "macro_rules! f{i} {{ () => {{ a0!(ignore); }}; }}\n"
        ));
    }
    shared.push_str("mod detail {\n    pub fn leaf() {}\n}\nignore!();\n");
    let old = format!("{shared}pub use detail::leaf as api;\n");
    let new = format!("{shared}pub use detail::leaf as renamed;\n");
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(!incomplete(&api), "{:?}", api.parse_truncations);
    assert_eq!(classify(&api, "renamed").0, "add", "{api:?}");
}

/// crate のマクロの引数の中で呼ぶ外部のマクロも、引数の名前のマクロの束縛を比べる。
#[test]
fn external_macros_inside_local_arguments_are_compared() {
    let manifest = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndispatcher = { path = \"dispatcher\" }\n";
    let dispatcher = "#[macro_export]\nmacro_rules! invoke {\n    ($callee:ident, $n:ident) => { $callee!($n); };\n}\n";
    let package = "[package]\nname = \"dispatcher\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    let head = "use dispatcher::invoke;\n\nmacro_rules! emit {\n    ($($t:tt)*) => { $($t)* };\n}\n\nmod a {\n    macro_rules! m {\n        ($n:ident) => {};\n    }\n    pub(crate) use m;\n}\nmod b {\n    macro_rules! m {\n        ($n:ident) => { fn $n() {} };\n    }\n    pub(crate) use m;\n}\n\n";
    let tail = "\nmod detail {\n    pub fn item() {}\n}\npub use detail::*;\nemit! { invoke!(chosen, item); }\n";
    let old = format!("{head}use a::m as chosen;\n{tail}");
    let new = format!("{head}use b::m as chosen;\n{tail}");
    let api = api_changes_with(
        manifest,
        &[
            ("src/lib.rs", old.as_str()),
            ("dispatcher/Cargo.toml", package),
            ("dispatcher/src/lib.rs", dispatcher),
        ],
        &[("src/lib.rs", new.as_str())],
    );
    assert!(unverified(&api, "item"), "{api:?}");
}

/// 深く入れ子にした呼び出しの長い引数も、引数を渡す呼び出しの判定は 1 度のなめで終わる。
#[test]
fn deeply_nested_calls_are_read_once() {
    let depth = 256;
    let payload = "x ".repeat(100_000);
    let rhs = format!("{}{payload}{};", "sink!(".repeat(depth), ")".repeat(depth));
    let shared = format!(
        "macro_rules! unused {{ () => {{ {rhs} }}; }}\nmod detail {{\n    pub fn leaf() {{}}\n}}\n"
    );
    let old = format!("{shared}pub use detail::leaf as api;\n");
    let new = format!("{shared}pub use detail::leaf as renamed;\n");
    let started = std::time::Instant::now();
    let api = api_changes(
        &[("src/lib.rs", old.as_str())],
        &[("src/lib.rs", new.as_str())],
    );
    assert_eq!(classify(&api, "renamed").0, "add", "{api:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
}

/// 名前の指し得る module が多過ぎる crate は、表を上限で打ち切り、探し切れなかったと申告する (上限より前に
/// 二乗の量を持たない。module を指さない名前の項目も、1 回の伝播の途中でも数える)。
#[test]
fn module_name_tables_are_bounded() {
    let n = 1500;
    let leaf = "mod detail {\n    pub fn leaf() {}\n}\n";
    // 曖昧な module の名前を多くの module へ glob で持ち込む。
    let mut ambiguous = String::new();
    for i in 0..n {
        ambiguous.push_str(&format!("mod a{i} {{ pub mod common {{}} }}\n"));
    }
    ambiguous.push_str("mod hub {\n");
    for i in 0..n {
        ambiguous.push_str(&format!("pub use crate::a{i}::*;\n"));
    }
    ambiguous.push_str("}\n");
    for i in 0..n {
        ambiguous.push_str(&format!("mod q{i} {{ pub use crate::hub::*; }}\n"));
    }
    // module を指さない多くの名前を多くの module へ glob で持ち込む。
    let mut items = String::from("mod hub {\n");
    for i in 0..n {
        items.push_str(&format!("pub struct S{i} {{}}\n"));
    }
    items.push_str("}\n");
    for i in 0..n {
        items.push_str(&format!("mod q{i} {{ pub use crate::hub::*; }}\n"));
    }
    // 1 回の伝播で多くの経路を写す (名前の取り込みを後から解く)。
    let mut bridge = String::new();
    for i in 0..n {
        bridge.push_str(&format!("mod a{i} {{ pub mod common {{}} }}\n"));
    }
    bridge.push_str("mod hub { pub mod common {}\n");
    for i in 0..n {
        bridge.push_str(&format!("pub use crate::a{i}::*;\n"));
    }
    bridge.push_str("}\n");
    for i in 0..n {
        bridge.push_str(&format!("mod q{i} {{ pub use crate::zbridge::*; }}\n"));
    }
    bridge.push_str("mod zbridge { pub use crate::hub::common; }\n");
    for (label, shared) in [
        ("ambiguous", ambiguous),
        ("items", items),
        ("bridge", bridge),
    ] {
        let old = format!("{shared}{leaf}pub use detail::leaf as api;\n");
        let new = format!("{shared}{leaf}pub use detail::leaf as renamed;\n");
        let started = std::time::Instant::now();
        let api = api_changes(
            &[("src/lib.rs", old.as_str())],
            &[("src/lib.rs", new.as_str())],
        );
        assert!(incomplete(&api), "{label}: {api:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(120),
            "{label}: {:?}",
            started.elapsed()
        );
    }
}

/// 外部のマクロ (`thread_local!`) の引数の型の名前は、crate のマクロに着かなければ、その取り込みの置き場が
/// 変わっても呼び出しの入力にしない (外部の glob のある module の名前でも)。
#[test]
fn external_macro_arguments_that_are_not_macros_are_not_inputs() {
    let manifest = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n\n[dependencies]\ndep = { path = \"dep\" }\n";
    let lib = "pub mod doctor;\npub mod engine;\npub mod language;\n";
    let parser = "use crate::language::LangId;\n\nthread_local! {\n    static POOL: std::cell::Cell<Option<LangId>> = const { std::cell::Cell::new(None) };\n}\n";
    let new_lib = format!("#[cfg(feature = \"x\")]\n{lib}");
    let api = api_changes_with(
        manifest,
        &[
            ("src/lib.rs", lib),
            (
                "src/language.rs",
                "pub use dep::*;\n\npub enum LangId {\n    Rust,\n}\n",
            ),
            (
                "src/doctor.rs",
                "use crate::language::LangId;\n\npub fn run() -> Option<LangId> {\n    None\n}\n",
            ),
            ("src/engine.rs", "pub mod parser;\n"),
            ("src/engine/parser.rs", parser),
            (
                "dep/Cargo.toml",
                "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("dep/src/lib.rs", "pub fn helper() {}\n"),
        ],
        &[("src/lib.rs", new_lib.as_str())],
    );
    assert_eq!(classify(&api, "LangId").0, "none", "{api:?}");
}
