//! Rust の module の宣言の条件 (`#[cfg] mod m;`) の変更で、配下の公開の項目の有る設定が変わる
//! (Issue #305)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

use crate::models::review::{ApiChanges, ApiModificationUnverifiedReason, CfgAvailability};
use crate::models::truncation::TruncationReason;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\ny = []\n";

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

/// 探し切れなかったことを申告したか。
fn incomplete(api: &ApiChanges) -> bool {
    api.parse_truncations
        .iter()
        .any(|truncation| truncation.reason == TruncationReason::RustModuleAvailabilityIncomplete)
}

const X: &str = "cfg(feature = \"x\")";

/// Issue の再現: 別のファイルに中身のある module の宣言に `#[cfg]` を足すと、変えていないファイルの
/// 公開の項目が消える設定がある。呼び出しが残れば削除 (blocking)。
#[test]
fn module_declaration_cfg_removes_items_of_unchanged_files() {
    let api_rs = ("src/api.rs", "pub fn make() -> u32 {\n    1\n}\n");
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), api_rs],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    assert!(!incomplete(&api), "{api:?}");
    // ほかの module から呼ぶ
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\npub mod user;\n"),
            api_rs,
            (
                "src/user.rs",
                "pub fn run() -> u32 {\n    crate::api::make()\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\npub mod user;\n"),
        )],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    // `#[path]` で読むファイル
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[path = \"imp/api_impl.rs\"]\npub mod api;\n",
            ),
            ("src/imp/api_impl.rs", "pub fn make() -> u32 {\n    1\n}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\n#[path = \"imp/api_impl.rs\"]\npub mod api;\n"),
        )],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    // `cfg` に絞った `cfg_attr`
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), api_rs],
        &[(
            "src/lib.rs",
            Some("#[cfg_attr(feature = \"y\", cfg(feature = \"x\"))]\npub mod api;\n"),
        )],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
}

/// 親の module の条件、途中のファイルの先頭の `#![cfg]`、inline module の中の `mod x;` の条件も
/// 置き場の条件に入る。
#[test]
fn ancestor_conditions_reach_nested_module_files() {
    let deep = ("src/outer/inner.rs", "pub fn deep() -> u32 {\n    1\n}\n");
    // 親の module の宣言の条件
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer;\n"),
            ("src/outer.rs", "pub mod inner;\n"),
            deep,
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod outer;\n"),
        )],
    );
    assert_eq!(
        classify(&api, "deep"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    // 途中のファイルの先頭の `#![cfg]`
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer;\n"),
            ("src/outer.rs", "pub mod inner;\n"),
            deep,
        ],
        &[(
            "src/outer.rs",
            Some("#![cfg(feature = \"x\")]\npub mod inner;\n"),
        )],
    );
    assert_eq!(
        classify(&api, "deep"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    // inline module の中の `mod x;`
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer {\n    pub mod inner;\n}\n"),
            deep,
        ],
        &[(
            "src/lib.rs",
            Some("pub mod outer {\n    #[cfg(feature = \"x\")]\n    pub mod inner;\n}\n"),
        )],
    );
    assert_eq!(
        classify(&api, "deep"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// 同値な書き換えと、条件を module から項目へ移すだけの変更は報告しない。条件を外せば増える設定。
#[test]
fn equivalent_module_conditions_are_not_reported() {
    let api_rs = ("src/api.rs", "pub fn make() -> u32 {\n    1\n}\n");
    let both = "#[cfg(all(feature = \"x\", feature = \"y\"))]\npub mod api;\n";
    for rewritten in [
        "#[cfg(all(feature = \"y\", feature = \"x\"))]\npub mod api;\n",
        "#[cfg(feature = \"x\")]\n#[cfg(feature = \"y\")]\npub mod api;\n",
    ] {
        let api = api_changes(
            &[("src/lib.rs", both), api_rs],
            &[("src/lib.rs", Some(rewritten))],
        );
        assert_eq!(
            classify(&api, "make"),
            ("none", None),
            "{rewritten}: {api:?}"
        );
        assert!(!incomplete(&api), "{api:?}");
    }
    // module の条件を項目へ移す
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod api;\n"),
            api_rs,
        ],
        &[
            ("src/lib.rs", Some("pub mod api;\n")),
            (
                "src/api.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() -> u32 {\n    1\n}\n"),
            ),
        ],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
    // 条件を外す
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod api;\n"),
            api_rs,
        ],
        &[("src/lib.rs", Some("pub mod api;\n"))],
    );
    assert_eq!(
        classify(&api, "make"),
        ("add", cfg(&[&[X]], &[&[]])),
        "{api:?}"
    );
}

/// cfg で分けた別々のファイルの module: 条件の入れ替え・分岐の削除を、置き場とファイルの対応で比べる。
#[test]
fn split_module_files_follow_their_own_conditions() {
    let split = "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n";
    let a_rs = ("src/a.rs", "pub fn only_a() {}\n\npub fn common() {}\n");
    let b_rs = ("src/b.rs", "pub fn common() {}\n");
    // 条件を入れ替える: only_a は消える設定と増える設定がある。common は全ての設定にある。
    let swapped = "#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod api;\n";
    let api = api_changes(
        &[("src/lib.rs", split), a_rs, b_rs],
        &[("src/lib.rs", Some(swapped))],
    );
    assert_eq!(classify(&api, "only_a").0, "rm_dead", "{api:?}");
    assert!(
        api.added.iter().any(|symbol| symbol.name == "only_a"),
        "{api:?}"
    );
    assert_eq!(classify(&api, "common"), ("none", None), "{api:?}");
    // 片方の分岐の宣言を消し、残ったファイルはコメントだけ変える
    let api = api_changes(
        &[("src/lib.rs", split), a_rs, b_rs],
        &[
            (
                "src/lib.rs",
                Some("#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n"),
            ),
            (
                "src/a.rs",
                Some("// unused\npub fn only_a() {}\n\npub fn common() {}\n"),
            ),
        ],
    );
    assert_eq!(classify(&api, "only_a").0, "rm_dead", "{api:?}");
    // 1 つのファイルにまとめ、使わなくなったファイルから項目を消す: 全ての設定に残る
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            a_rs,
            ("src/b.rs", "pub fn only_a() {}\n\npub fn common() {}\n"),
        ],
        &[
            ("src/lib.rs", Some("#[path = \"b.rs\"]\npub mod api;\n")),
            ("src/a.rs", Some("pub fn common() {}\n")),
        ],
    );
    assert_eq!(classify(&api, "only_a"), ("none", None), "{api:?}");
    assert_eq!(classify(&api, "common"), ("none", None), "{api:?}");
}

/// 置き場の付け替えで、同じ設定の契約が別のファイルの宣言に切り替わるなら組を決められない変更。
#[test]
fn switching_module_files_changes_contracts() {
    let split = "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            ("src/a.rs", "pub fn make(_: u8) {}\n"),
            ("src/b.rs", "pub fn make(_: u16) {}\n"),
        ],
        &[("src/lib.rs", Some("#[path = \"b.rs\"]\npub mod api;\n"))],
    );
    let change = api
        .modified
        .iter()
        .find(|change| change.name == "make")
        .unwrap_or_else(|| panic!("{api:?}"));
    assert_eq!(
        change
            .unverified
            .as_ref()
            .map(|unverified| unverified.reason),
        Some(ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous),
        "{api:?}"
    );
}

/// 非公開の分岐の置き場は、公開の分岐の置き場が消える設定を覆わない。
#[test]
fn private_module_branches_do_not_cover_public_ones() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\nmod api;\n",
            ),
            ("src/a.rs", "pub fn make() {}\n"),
            ("src/b.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some(
                "#[cfg(all(feature = \"x\", feature = \"y\"))]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(all(feature = \"x\", feature = \"y\")))]\n#[path = \"b.rs\"]\nmod api;\n",
            ),
        )],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
}

/// どの設定でも無い module の先は、ファイルが無くても探し切れないことにしない。
#[test]
fn unsatisfiable_module_conditions_need_no_files() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg(any())]\npub mod missing;\npub mod api;\n",
            ),
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(all(any(), feature = \"x\"))]\npub mod missing;\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// 場所を決められない宣言は、場所の指定か条件が変わったときだけ探し切れないことにする。
#[test]
fn undeterminable_module_files_block_only_when_they_change() {
    let api_rs = ("src/api.rs", "pub fn make() {}\n");
    let platform = ("src/platform.rs", "pub fn native() {}\n");
    // `cfg_attr` の中の `path` を変える
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg_attr(feature = \"x\", path = \"platform.rs\")]\npub mod sys;\n",
            ),
            platform,
            ("src/sys.rs", "pub fn native() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg_attr(feature = \"x\", path = \"sys.rs\")]\npub mod sys;\n"),
        )],
    );
    assert!(incomplete(&api), "{api:?}");
    // 無関係な module の場所を決められない
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg_attr(feature = \"y\", path = \"platform.rs\")]\nmod sys;\npub mod api;\n",
            ),
            platform,
            ("src/sys.rs", "pub fn native() {}\n"),
            api_rs,
        ],
        &[(
            "src/lib.rs",
            Some(
                "#[cfg_attr(feature = \"y\", path = \"platform.rs\")]\nmod sys;\n#[cfg(feature = \"x\")]\npub mod api;\n",
            ),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// 条件の変わった module の項目の位置のマクロは項目を生成し得るので、探し切れないことにする。
/// 無関係な module のマクロは止めない。
#[test]
fn macros_in_changed_modules_block() {
    let outer = (
        "src/outer.rs",
        "macro_rules! expose {\n    () => {\n        pub mod generated {\n            pub fn generated() {}\n        }\n    };\n}\n\nexpose!();\n",
    );
    let api = api_changes(
        &[("src/lib.rs", "pub mod outer;\n"), outer],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod outer;\n"),
        )],
    );
    assert!(incomplete(&api), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer;\npub mod api;\n"),
            outer,
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("pub mod outer;\n#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
}

/// 別のファイルの module を、条件付きの inline module に書き換える (元のファイルは残す)。
#[test]
fn file_modules_rewritten_inline_are_compared() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api {\n    pub fn make() {}\n}\n"),
        )],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// lib が `#[path]` で読む別の target の場所のファイルと、2 つの module に使うファイル。
#[test]
fn files_outside_the_library_layout_and_shared_files() {
    let api = api_changes(
        &[
            ("src/lib.rs", "#[path = \"bin/helper.rs\"]\npub mod api;\n"),
            (
                "src/bin/helper.rs",
                "#[path = \"../leaf.rs\"]\npub mod leaf;\n\n#[allow(dead_code)]\nfn main() {}\n",
            ),
            ("src/leaf.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/bin/helper.rs",
            Some(
                "#[cfg(feature = \"x\")]\n#[path = \"../leaf.rs\"]\npub mod leaf;\n\n#[allow(dead_code)]\nfn main() {}\n",
            ),
        )],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    // 2 つの module に使うファイルの片方の宣言の条件
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[path = \"shared.rs\"]\npub mod a;\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
            ("src/shared.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some(
                "#[cfg(feature = \"x\")]\n#[path = \"shared.rs\"]\npub mod a;\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
        )],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// `files` をコミットし、`changes` に書き換えたときの API の差分 (`manifest` を Cargo.toml にする)。
fn api_changes_with(
    manifest: &str,
    files: &[(&str, &str)],
    changes: &[(&str, Option<&str>)],
) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut all = vec![("Cargo.toml", manifest)];
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

/// どの設定でも成り立たない条件を足す・外すのも、置き場の条件の変化 (配下の子の module も)。
#[test]
fn never_true_module_conditions_are_compared() {
    let files = [
        ("src/lib.rs", "pub mod api;\n"),
        ("src/api.rs", "pub fn make() {}\npub mod inner;\n"),
        ("src/api/inner.rs", "pub fn deep() {}\n"),
    ];
    let api = api_changes(
        &files,
        &[("src/lib.rs", Some("#[cfg(any())]\npub mod api;\n"))],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[])),
        "{api:?}"
    );
    assert_eq!(
        classify(&api, "deep"),
        ("rm_dead", cfg(&[&[]], &[])),
        "{api:?}"
    );
    assert!(!incomplete(&api), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(any())]\npub mod api;\n"),
            files[1],
            files[2],
        ],
        &[("src/lib.rs", Some("pub mod api;\n"))],
    );
    assert_eq!(classify(&api, "make"), ("add", cfg(&[], &[&[]])), "{api:?}");
}

/// 同じファイルを inline module の中身から別の module のファイルに組み替えて契約を変えると、ファイル
/// ごとの比較は削除と追加に分かれるので、組を決められない変更にする。
#[test]
fn regrouped_files_keep_their_contract_changes() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "pub mod inner {\n    pub fn make(_: u8) {}\n}\n",
            ),
        ],
        &[
            (
                "src/lib.rs",
                Some("pub mod api {\n    #[path = \"../api.rs\"]\n    pub mod inner;\n}\n"),
            ),
            ("src/api.rs", Some("pub fn make(_: u16) {}\n")),
        ],
    );
    let change = api
        .modified
        .iter()
        .find(|change| change.name == "make")
        .unwrap_or_else(|| panic!("{api:?}"));
    assert_eq!(
        change
            .unverified
            .as_ref()
            .map(|unverified| unverified.reason),
        Some(ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous),
        "{api:?}"
    );
}

/// 非公開の inline module の項目は、置き場の条件の同値な書き換えで報告しない。
#[test]
fn private_inline_module_items_follow_equivalent_rewrites() {
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod api;\n"),
            (
                "src/api.rs",
                "mod hidden {\n    pub fn helper() {}\n}\n\npub fn run() {\n    hidden::helper();\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(all(feature = \"x\"))]\npub mod api;\n"),
        )],
    );
    assert_eq!(classify(&api, "helper"), ("none", None), "{api:?}");
    assert_eq!(classify(&api, "run"), ("none", None), "{api:?}");
}

/// module の宣言に関わらない変更 (テストの追加・`#[doc]` の値) では、場所を決められない宣言や
/// `[lib] path` があっても止めない。
#[test]
fn unrelated_changes_do_not_open_the_module_gate() {
    let custom = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/custom.rs\"\n";
    let api = api_changes_with(
        custom,
        &[("src/custom.rs", "pub fn make() {}\n")],
        &[("tests/smoke.rs", Some("#[test]\nfn smoke() {}\n"))],
    );
    assert!(!incomplete(&api), "{api:?}");
    let undetermined = [
        (
            "src/lib.rs",
            "#[doc = \"old\"]\n#[cfg_attr(feature = \"x\", path = \"alt.rs\")]\npub mod api;\n",
        ),
        ("src/api.rs", "pub fn make() {}\n"),
        ("src/alt.rs", "pub fn make() {}\n"),
    ];
    let api = api_changes(
        &undetermined,
        &[("tests/smoke.rs", Some("#[test]\nfn smoke() {}\n"))],
    );
    assert!(!incomplete(&api), "{api:?}");
    let api = api_changes(
        &undetermined,
        &[(
            "src/lib.rs",
            Some(
                "#[doc = \"new\"]\n#[cfg_attr(feature = \"x\", path = \"alt.rs\")]\npub mod api;\n",
            ),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
}

/// 2 つの module に使うファイルの、条件の違う 2 つの変化は別々に残す。
#[test]
fn shared_files_report_each_module() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[path = \"shared.rs\"]\npub mod a;\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
            ("src/shared.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some(
                "#[cfg(feature = \"x\")]\n#[path = \"shared.rs\"]\npub mod a;\n#[cfg(feature = \"y\")]\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
        )],
    );
    let reported: Vec<_> = api
        .removed_dead
        .iter()
        .filter(|symbol| symbol.name == "make")
        .map(|symbol| symbol.cfg_availability.clone())
        .collect();
    assert_eq!(
        reported,
        vec![
            cfg(&[&[]], &[&[X]]),
            cfg(&[&[]], &[&["cfg(feature = \"y\")"]])
        ],
        "{api:?}"
    );
}

/// `name` の報告 (削除・追加) の件数。
fn reports(api: &ApiChanges, name: &str) -> (usize, usize) {
    let count = |symbols: &[crate::models::review::ApiSymbol]| {
        symbols.iter().filter(|symbol| symbol.name == name).count()
    };
    (
        count(&api.removed) + count(&api.removed_dead),
        count(&api.added),
    )
}

/// 別のファイルの宣言に切り替わるとき、signature の字面が同じでも、名指す名前の束縛が違えば契約の
/// 変化 (同じなら報告しない)。
#[test]
fn switching_module_files_compares_bindings() {
    let split = "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n";
    let swapped = "#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod api;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            ("src/a.rs", "type Arg = u8;\npub fn make(_: Arg) {}\n"),
            ("src/b.rs", "type Arg = u16;\npub fn make(_: Arg) {}\n"),
        ],
        &[("src/lib.rs", Some(swapped))],
    );
    assert_eq!(classify(&api, "make").0, "mod", "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            ("src/a.rs", "type Arg = u8;\npub fn make(_: Arg) {}\n"),
            ("src/b.rs", "type Arg = u8;\npub fn make(_: Arg) {}\n"),
        ],
        &[("src/lib.rs", Some(swapped))],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
    // inline module の中身から別の module のファイルへの組み替え: 束縛が同じなら報告しない
    let regrouped = (
        "src/lib.rs",
        Some("pub mod api {\n    #[path = \"../api.rs\"]\n    pub mod inner;\n}\n"),
    );
    let before = [
        ("src/lib.rs", "pub mod api;\n"),
        (
            "src/api.rs",
            "pub mod inner {\n    type T = u8;\n    pub fn make(_: T) {}\n}\n",
        ),
    ];
    let api = api_changes(
        &before,
        &[
            regrouped,
            ("src/api.rs", Some("type T = u8;\npub fn make(_: T) {}\n")),
        ],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
    let api = api_changes(
        &before,
        &[
            regrouped,
            ("src/api.rs", Some("type T = u16;\npub fn make(_: T) {}\n")),
        ],
    );
    assert_eq!(classify(&api, "make").0, "mod", "{api:?}");
}

/// impl の中身の項目の位置のマクロは項目を生成し得るので探し切れない。関数の本体の中のマクロは
/// 止めない。
#[test]
fn macros_inside_impls_block_but_not_inside_bodies() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub struct S;\npub mod api;\n"),
            (
                "src/api.rs",
                "macro_rules! methods {\n    () => {\n        pub fn generated() {}\n    };\n}\n\nimpl crate::S {\n    methods!();\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("pub struct S;\n#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert!(incomplete(&api), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "pub fn make() -> String {\n    format!(\"{}\", 1)\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
}

/// 非公開の inline module の関数は、置き場の条件を狭めても報告しない。同じファイルの `pub use` で
/// 公開していれば報告する。
#[test]
fn private_inline_module_items_count_only_when_reexported() {
    let narrowed = (
        "src/lib.rs",
        Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
    );
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "mod hidden {\n    pub fn helper() {}\n}\n\nfn run() {\n    hidden::helper();\n}\n",
            ),
        ],
        &[narrowed],
    );
    assert_eq!(classify(&api, "helper"), ("none", None), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "mod hidden {\n    pub fn helper() {}\n}\n\npub use hidden::helper;\n",
            ),
        ],
        &[narrowed],
    );
    assert_ne!(classify(&api, "helper").0, "none", "{api:?}");
}

/// どの設定でも着かない置き場のファイルのマクロや中身は、探し切れないことにしない。テストの中の
/// module や `cfg_attr` の中の doc の変更でも止めない。
#[test]
fn unreachable_and_unrelated_inputs_do_not_block() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg(any())]\n#[path = \"a.rs\"]\npub mod unused;\npub mod api;\n",
            ),
            ("src/a.rs", "include!(\"missing.rs\");\n"),
            ("src/b.rs", "\n"),
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(any())]\n#[path = \"b.rs\"]\npub mod unused;\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    let custom = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/custom.rs\"\n";
    let api = api_changes_with(
        custom,
        &[("src/custom.rs", "pub fn make() {}\n")],
        &[(
            "tests/smoke.rs",
            Some("mod helpers {}\n\n#[test]\nfn smoke() {}\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    let undetermined = |doc: &str| {
        format!(
            "#[cfg_attr(feature = \"y\", doc = \"{doc}\")]\n#[cfg_attr(feature = \"x\", path = \"alt.rs\")]\npub mod api;\n"
        )
    };
    let old = undetermined("old");
    let new = undetermined("new");
    let api = api_changes(
        &[
            ("src/lib.rs", &old),
            ("src/api.rs", "pub fn make() {}\n"),
            ("src/alt.rs", "pub fn make() {}\n"),
        ],
        &[("src/lib.rs", Some(&new))],
    );
    assert!(!incomplete(&api), "{api:?}");
}

/// `[lib] path` の crate root からもたどる。
#[test]
fn custom_library_roots_are_followed() {
    let custom = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n\n[lib]\npath = \"lib/root.rs\"\n";
    let api = api_changes_with(
        custom,
        &[
            ("lib/root.rs", "pub mod api;\n"),
            ("lib/api.rs", "pub fn make() {}\n"),
        ],
        &[(
            "lib/root.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// 2 つの module に使うファイルの報告: 追加の向きも条件ごとに残し、同じ条件の報告は重ねない。
#[test]
fn shared_files_report_each_condition_once() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\n#[path = \"shared.rs\"]\npub mod a;\n#[cfg(feature = \"y\")]\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
            ("src/shared.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[path = \"shared.rs\"]\npub mod a;\n#[path = \"shared.rs\"]\npub mod b;\n"),
        )],
    );
    assert_eq!(reports(&api, "make"), (0, 2), "{api:?}");
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[path = \"shared.rs\"]\npub mod a;\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
            ("src/shared.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some(
                "#[cfg(feature = \"x\")]\n#[path = \"shared.rs\"]\npub mod a;\n#[cfg(feature = \"x\")]\n#[path = \"shared.rs\"]\npub mod b;\n",
            ),
        )],
    );
    assert_eq!(reports(&api, "make"), (1, 0), "{api:?}");
}

/// 非公開の inline module を module ごと `pub use` で再公開すると、配下の項目も公開。
#[test]
fn module_reexports_expose_private_inline_items() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "mod hidden {\n    pub mod inner {\n        pub fn make() {}\n    }\n}\n\npub use hidden::inner as exposed;\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert_ne!(classify(&api, "make").0, "none", "{api:?}");
}

/// 別のファイルの宣言への切り替えでも、名指す型の定義を公開の経路を保って移しただけなら契約は同じ。
#[test]
fn switching_module_files_follows_moved_definitions() {
    let split = "pub mod types;\npub mod relocated;\n\n#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n";
    let swapped = "pub mod types;\npub mod relocated;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod api;\n";
    let body = "use crate::types::T;\npub fn make(_: T) {}\n";
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            ("src/a.rs", body),
            ("src/b.rs", body),
            ("src/types.rs", "pub struct T;\n"),
            ("src/relocated.rs", "\n"),
        ],
        &[
            ("src/lib.rs", Some(swapped)),
            ("src/types.rs", Some("pub use crate::relocated::T;\n")),
            ("src/relocated.rs", Some("pub struct T;\n")),
        ],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// `cfg_attr` の中の `path` と並ぶ属性の変更、どの設定でも無い inline module の中のマクロ、どの設定でも
/// 着かない大きなファイルでは止めない。
#[test]
fn inputs_that_cannot_matter_do_not_block() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg_attr(feature = \"x\", path = \"alt.rs\", doc = \"old\")]\npub mod api;\n",
            ),
            ("src/api.rs", "pub fn make() {}\n"),
            ("src/alt.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg_attr(feature = \"x\", path = \"alt.rs\", doc = \"new\")]\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "pub fn make() {}\n\n#[cfg(any())]\nmod unused {\n    include!(\"missing.rs\");\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    let large = format!("// {}\n", "x".repeat(2 * 1024 * 1024 + 16));
    let custom = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/custom.rs\"\n";
    let api = api_changes_with(
        custom,
        &[
            (
                "src/custom.rs",
                "#[cfg(any())]\n#[path = \"a.rs\"]\npub mod unused;\n",
            ),
            ("src/a.rs", &large),
            ("src/b.rs", "\n"),
        ],
        &[(
            "src/custom.rs",
            Some("#[cfg(any())]\n#[path = \"b.rs\"]\npub mod unused;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
}

/// `r#cfg` も `cfg` として数える。
#[test]
fn raw_cfg_attributes_are_conditions() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[r#cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
}

/// 別のファイルの宣言への切り替えでは、signature が先頭の行だけの型別名の右辺も比べる。
#[test]
fn switching_module_files_compares_whole_declarations() {
    let split = "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n";
    let swapped = "#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod api;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            ("src/a.rs", "pub type Arg =\n    u8;\n"),
            ("src/b.rs", "pub type Arg =\n    u16;\n"),
        ],
        &[("src/lib.rs", Some(swapped))],
    );
    assert_eq!(classify(&api, "Arg").0, "mod", "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            ("src/a.rs", "pub type Arg =\n    u8;\n"),
            ("src/b.rs", "pub type Arg =\n    u8;\n"),
        ],
        &[("src/lib.rs", Some(swapped))],
    );
    assert_eq!(classify(&api, "Arg"), ("none", None), "{api:?}");
}

/// どの設定でも無い inline module (宣言の外側の属性・中身の先頭の内側の属性・置き場の条件との組み合わせ)
/// の中のマクロは、探し切れないことにも、非公開の項目を公開に数える理由にもしない。
#[test]
fn macros_in_modules_that_never_exist_are_ignored() {
    let narrowed = (
        "src/lib.rs",
        Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
    );
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "mod hidden {\n    pub fn helper() {}\n}\n\nfn run() {\n    hidden::helper();\n}\n\n#[cfg(any())]\nmod unused {\n    include!(\"missing.rs\");\n}\n",
            ),
        ],
        &[narrowed],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(classify(&api, "helper"), ("none", None), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "pub fn make() {}\n\nmod unused {\n    #![cfg(any())]\n    include!(\"missing.rs\");\n}\n",
            ),
        ],
        &[narrowed],
    );
    assert!(!incomplete(&api), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod api;\n"),
            (
                "src/api.rs",
                "pub fn make() {}\n\n#[cfg(not(feature = \"x\"))]\nmod unused {\n    include!(\"missing.rs\");\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(all(feature = \"x\"))]\npub mod api;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// 別のファイルの関数への切り替えで、束縛側の `mut` だけの違いは契約の変化にしない。
#[test]
fn switching_module_files_ignores_binding_mutability() {
    let split = "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n";
    let swapped = "#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod api;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            (
                "src/a.rs",
                "pub fn make(mut value: u8) {\n    value += 1;\n}\n",
            ),
            (
                "src/b.rs",
                "pub fn make(value: u8) {\n    let _ = value;\n}\n",
            ),
        ],
        &[("src/lib.rs", Some(swapped))],
    );
    assert_eq!(classify(&api, "make"), ("none", None), "{api:?}");
}

/// ファイル自身の先頭の `#![cfg]` でどの設定でも無いファイルのマクロは、探し切れないことにしない。
#[test]
fn macros_in_files_that_never_exist_are_ignored() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\npub mod other;\n"),
            ("src/api.rs", "#![cfg(any())]\ninclude!(\"missing.rs\");\n"),
            ("src/other.rs", "pub fn make() {}\n"),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\npub mod other;\n"),
        )],
    );
    assert!(!incomplete(&api), "{api:?}");
}

/// 別のファイルの trait への切り替えでは、関連型の制約も比べる (メソッドの本体の違いは比べない)。
#[test]
fn switching_module_files_compares_trait_associated_types() {
    let split = "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod api;\n";
    let merged = "#[path = \"b.rs\"]\npub mod api;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            ("src/a.rs", "pub trait Api {\n    type Value;\n}\n"),
            ("src/b.rs", "pub trait Api {\n    type Value: Copy;\n}\n"),
        ],
        &[("src/lib.rs", Some(merged))],
    );
    assert_eq!(classify(&api, "Api").0, "mod", "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", split),
            (
                "src/a.rs",
                "pub trait Api {\n    type Value: Copy;\n    fn get(&self) -> u8 {\n        1\n    }\n}\n",
            ),
            (
                "src/b.rs",
                "pub trait Api {\n    type Value: Copy;\n    fn get(&self) -> u8 {\n        2\n    }\n}\n",
            ),
        ],
        &[("src/lib.rs", Some(merged))],
    );
    assert_eq!(classify(&api, "Api"), ("none", None), "{api:?}");
}
