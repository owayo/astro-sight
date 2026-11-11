//! Rust の module の宣言の条件 (`#[cfg] mod m;`) の変更で、配下の公開の項目の有る設定が変わる
//! (Issue #305)。module の宣言を消す・足すときも同じく比べる (Issue #314)。

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

/// Issue #314: 別のファイルに中身のある公開の module の宣言を消すと、変えていないファイルの公開の
/// 項目は利用者から使えなくなる。呼び出しが残れば削除 (blocking)、残らなければ rm_dead。入れ子の
/// module のファイル、inline module の中の宣言、条件付きの宣言も同じ。宣言を足せば追加。
#[test]
fn removed_module_declarations_remove_items_of_unchanged_files() {
    let make = ("src/api.rs", "pub fn make() -> u32 {\n    1\n}\n");
    // Issue の再現
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[]], &[])),
        "{api:?}"
    );
    // 呼び出しが残れば削除
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            make,
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    assert_eq!(notes::api::make(), 1);\n}\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm", "{api:?}");
    // 入れ子の module のファイルも
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", "pub mod inner;\n"),
            ("src/api/inner.rs", "pub fn deep() {}\n"),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "deep").0, "rm_dead", "{api:?}");
    // inline module の中の宣言
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer {\n    pub mod api;\n}\n"),
            ("src/outer/api.rs", "pub fn make() -> u32 {\n    1\n}\n"),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    // 条件付きの宣言
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod api;\n"),
            make,
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[X]], &[])),
        "{api:?}"
    );
    // 宣言を足せば追加
    let api = api_changes(
        &[("src/lib.rs", ""), make],
        &[("src/lib.rs", Some("pub mod api;\n"))],
    );
    assert_eq!(classify(&api, "make").0, "add", "{api:?}");
    // 別の経路で宣言し直せば、旧い経路の削除と新しい経路の追加
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make],
        &[("src/lib.rs", Some("#[path = \"api.rs\"]\npub mod v2;\n"))],
    );
    assert_eq!(reports(&api, "make"), (1, 1), "{api:?}");
    // ファイルも消せば、ファイルごとの削除として 1 度だけ出す
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make],
        &[("src/lib.rs", Some("")), ("src/api.rs", None)],
    );
    assert_eq!(reports(&api, "make"), (1, 0), "{api:?}");
}

/// module の宣言を消すとき、そのファイルの項目を探し切れなければ (構文エラー、項目の位置のマクロ)、
/// 見逃さずに探し切れなかったことを申告する (Issue #314)。
#[test]
fn removed_modules_that_cannot_be_listed_block() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", "pub fn make() -> u32 {\n    1\n\n"),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert!(incomplete(&api), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "macro_rules! define {\n    () => {\n        pub fn made() {}\n    };\n}\n\ndefine!();\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert!(incomplete(&api), "{api:?}");
}

/// 消した module の名前を、新しい版の公開の取り込み (`pub use detail as api;`・公開の glob) が作り得るなら、
/// 項目の削除にせず再公開の名前の比較 (#306) に任せる。どの名前も作り得るもの (項目の位置のマクロ) が
/// あれば、探し切れなかったことを申告する。項目の `#[cfg]` も変えれば、module の経路が消えた削除と、宣言
/// 自身の条件の変化 (ファイルごとの比較) を別の事実として出す。同じ名前の再公開の削除とは重ねない
/// (Issue #314 のレビュー)。
#[test]
fn removed_modules_rebound_by_imports_or_reported() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    let uses = (
        "tests/use_make.rs",
        "#[test]\nfn uses() {\n    notes::api::make();\n}\n",
    );
    // 別の名前の module にして、旧い公開の経路を再公開で保つ
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make, uses],
        &[(
            "src/lib.rs",
            Some("#[path = \"api.rs\"]\nmod detail;\npub use detail as api;\n"),
        )],
    );
    assert_eq!(reports(&api, "make"), (0, 0), "{api:?}");
    // 公開の glob が名前を持ち込み得る
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make, uses],
        &[(
            "src/lib.rs",
            Some(
                "mod inner {\n    #[path = \"../api.rs\"]\n    pub mod api;\n}\npub use inner::*;\n",
            ),
        )],
    );
    assert_eq!(reports(&api, "make"), (0, 0), "{api:?}");
    // 項目の位置のマクロが名前を作り得る
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make],
        &[(
            "src/lib.rs",
            Some("macro_rules! nothing {\n    () => {};\n}\n\nnothing!();\n"),
        )],
    );
    assert!(incomplete(&api), "{api:?}");
    assert_eq!(reports(&api, "make"), (0, 0), "{api:?}");
    // 項目の `#[cfg]` も変えたとき、module の経路の削除と宣言自身の条件の変化
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make],
        &[
            ("src/lib.rs", Some("")),
            (
                "src/api.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    assert_eq!(reports(&api, "make"), (2, 0), "{api:?}");
    let conditions = removal_conditions(&api, "make");
    assert!(conditions.contains(&cfg(&[&[]], &[])), "{api:?}");
    assert!(conditions.contains(&cfg(&[&[]], &[&[X]])), "{api:?}");
    // 同じ名前の再公開も消したとき、削除は 1 度だけ
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\npub use api::make;\n"), make],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(reports(&api, "make"), (1, 0), "{api:?}");
}

/// 消した module の名前を、新しい版の crate の名前解決で親の module の全ての分岐から引く。持ち込めない
/// glob と、どの設定でも無い取り込みは名前を作らない (削除)。制限付きの取り込みと、`#[cfg]` で分けた親の
/// 片方の分岐の取り込みは名前を作る (再公開の名前の比較に任せる)。比べ直していない crate が同じファイルを
/// 使うときは、ファイルごとの有る設定の比較を残し、rename したファイルでも削除を重ねない (Issue #314 の
/// 2 回目のレビュー)。
#[test]
fn removed_module_names_are_resolved_in_the_crate() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    let uses = (
        "tests/use_make.rs",
        "#[test]\nfn uses() {\n    notes::api::make();\n}\n",
    );
    // `api` を持ち込めない公開の glob
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub mod api;\nmod other {}\npub use self::other::*;\n",
            ),
            make,
            uses,
        ],
        &[(
            "src/lib.rs",
            Some("mod other {}\npub use self::other::*;\n"),
        )],
    );
    assert_eq!(classify(&api, "make").0, "rm", "{api:?}");
    // どの設定でも無い取り込み
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\nmod other {}\n"), make, uses],
        &[(
            "src/lib.rs",
            Some("mod other {}\n#[cfg(any())]\npub use self::other as api;\n"),
        )],
    );
    assert_eq!(classify(&api, "make").0, "rm", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 制限付きの取り込みを経て、項目の公開の経路を保つ
    let api = api_changes(
        &[
            ("src/lib.rs", "mod api;\npub use api::make;\n"),
            make,
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::make();\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some(
                "#[path = \"api.rs\"]\nmod detail;\npub(crate) use self::detail as api;\npub use api::make;\n",
            ),
        )],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    // `#[cfg]` でファイルと inline に分けた親の、inline 側の取り込み
    let parent = "#[cfg(feature = \"x\")]\npub mod parent;\n#[cfg(not(feature = \"x\"))]\npub mod parent {\n";
    let api = api_changes(
        &[
            ("src/lib.rs", &format!("{parent}    pub mod api;\n}}\n")),
            ("src/parent.rs", ""),
            ("src/parent/api.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::parent::api::make();\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some(&format!(
                "{parent}    #[path = \"api.rs\"]\n    pub mod detail;\n    pub use self::detail as api;\n}}\n"
            )),
        )],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 比べ直していない crate (b) が同じファイルを使う
    let api = api_changes_with(
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        &[
            (
                "a/Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n",
            ),
            (
                "b/Cargo.toml",
                "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n",
            ),
            ("a/src/lib.rs", "#[path = \"shared.rs\"]\nmod hidden;\n"),
            (
                "b/src/lib.rs",
                "#[path = \"../../a/src/shared.rs\"]\npub mod api;\n",
            ),
            ("a/src/shared.rs", "pub fn make() {}\n"),
            (
                "b/tests/use_make.rs",
                "#[test]\nfn uses() {\n    b::api::make();\n}\n",
            ),
        ],
        &[
            ("a/src/lib.rs", Some("")),
            (
                "a/src/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm", cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// module の宣言を消し、ファイルを rename して項目の `#[cfg]` も変えるとき、module の経路の削除と、rename の
/// 対応で比べた宣言自身の条件の変化を出す (Issue #314 の 2 回目のレビュー)。
#[test]
fn removed_modules_with_renamed_files_report_both_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let helpers: String = (0..12)
        .map(|index| format!("pub fn helper{index}() -> u32 {{\n    {index}\n}}\n\n"))
        .collect();
    let old_api = format!("{helpers}pub fn make() {{}}\n");
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", &old_api),
        ],
        "base",
    );
    fs::write(repo.join("src/lib.rs"), "").expect("write");
    fs::remove_file(repo.join("src/api.rs")).expect("remove");
    fs::write(
        repo.join("src/unused.rs"),
        format!("{helpers}#[cfg(feature = \"x\")]\npub fn make() {{}}\n"),
    )
    .expect("write");
    git_in(repo, &["add", "-A"]);
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(reports(&api, "make"), (2, 0), "{api:?}");
    let conditions = removal_conditions(&api, "make");
    assert!(conditions.contains(&cfg(&[&[]], &[])), "{api:?}");
    assert!(conditions.contains(&cfg(&[&[]], &[&[X]])), "{api:?}");
}

/// `name` の削除 (removed・removed_dead) に添えた有る条件の並び。
fn removal_conditions(api: &ApiChanges, name: &str) -> Vec<Option<CfgAvailability>> {
    api.removed
        .iter()
        .chain(&api.removed_dead)
        .filter(|symbol| symbol.name == name)
        .map(|symbol| symbol.cfg_availability.clone())
        .collect()
}

/// 消した module の名前を、再公開の名前の比較 (#306) が取り込みの束縛として比べたかで決める。宣言が
/// 名前を作り直しても配下の項目は消え、何段もの glob が持ち込む名前は #306 に任せる (Issue #314 の
/// 3 回目のレビュー)。
#[test]
fn removed_module_names_follow_the_reexport_comparison() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    let uses = (
        "tests/use_make.rs",
        "#[test]\nfn uses() {\n    notes::api::make();\n}\n",
    );
    // 値の宣言が同じ名前を作る
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make, uses],
        &[("src/lib.rs", Some("pub fn api() {}\n"))],
    );
    assert_eq!(classify(&api, "make").0, "rm", "{api:?}");
    // 何段もの glob が、別名の module を持ち込む
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make, uses],
        &[(
            "src/lib.rs",
            Some(
                "#[path = \"api.rs\"]\npub mod detail;\n\nmod names {\n    pub use crate::detail as api;\n}\nmod bridge {\n    pub use crate::names::*;\n}\npub use crate::bridge::*;\n",
            ),
        )],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// 2 つの module に置いたファイルの片方の宣言を消すとき、残る置き場の削除 (ファイルごとの比較) と、残る
/// 経路の再公開の削除は、消した置き場の削除と別に残す (Issue #314 の 3 回目のレビュー)。
#[test]
fn removed_module_placements_keep_the_remaining_reports() {
    let lib = "#[cfg(feature = \"x\")]\n#[path = \"shared.rs\"]\npub mod a;\n#[path = \"shared.rs\"]\npub mod b;\n";
    // 残る置き場 `b` で、項目の `#[cfg]` を足して消える設定
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/shared.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::b::make();\n}\n",
            ),
        ],
        &[
            ("src/lib.rs", Some("#[path = \"shared.rs\"]\npub mod b;\n")),
            (
                "src/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    let conditions = removal_conditions(&api, "make");
    assert!(conditions.contains(&cfg(&[&[]], &[&[X]])), "{api:?}");
    assert!(conditions.contains(&cfg(&[&[X]], &[])), "{api:?}");
    // 残る置き場からの再公開も消す
    let api = api_changes(
        &[
            ("src/lib.rs", &format!("{lib}pub use b::make;\n")),
            ("src/shared.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::make();\n}\n",
            ),
        ],
        &[("src/lib.rs", Some("#[path = \"shared.rs\"]\npub mod b;\n"))],
    );
    let conditions = removal_conditions(&api, "make");
    assert!(conditions.contains(&None), "{api:?}");
    assert!(conditions.contains(&cfg(&[&[X]], &[])), "{api:?}");
}

/// module の宣言を消したファイルを、比べ直していない workspace の member が `#[path]` で公開しているなら、
/// その置き場のファイルごとの比較を残す (Issue #314 の 3 回目のレビュー)。
#[test]
fn removed_module_files_placed_by_other_crates_keep_file_reports() {
    let api = api_changes_with(
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        &[
            (
                "a/Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n",
            ),
            (
                "b/Cargo.toml",
                "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n",
            ),
            ("a/src/lib.rs", "#[path = \"shared.rs\"]\npub mod api;\n"),
            (
                "b/src/lib.rs",
                "#[path = \"../../a/src/shared.rs\"]\npub mod api;\n",
            ),
            ("a/src/shared.rs", "pub fn make() {}\n"),
            (
                "b/tests/use_make.rs",
                "#[test]\nfn uses() {\n    b::api::make();\n}\n",
            ),
        ],
        &[
            ("a/src/lib.rs", Some("")),
            (
                "a/src/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    let conditions = removal_conditions(&api, "make");
    assert!(conditions.contains(&cfg(&[&[]], &[])), "{api:?}");
    assert!(conditions.contains(&cfg(&[&[]], &[&[X]])), "{api:?}");
}

/// 消した module と同じ名前の値の取り込みは、module の名前 (型の名前空間) を作らないので、配下の項目を
/// 比べる (Issue #314 の 4 回目のレビュー)。
#[test]
fn removed_module_names_bound_only_as_values_are_compared() {
    let helper = "mod helper {\n    pub fn api() {}\n}\npub use helper::api;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", &format!("pub mod api;\n{helper}")),
            ("src/api.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::api::make();\n}\n",
            ),
        ],
        &[("src/lib.rs", Some(&format!("pub enum api {{}}\n{helper}")))],
    );
    assert_eq!(classify(&api, "make").0, "rm", "{api:?}");
}

/// 比べ直していない workspace の member が `include!` で同じファイルを読むか、manifest だけで lib の root を
/// 替えて旧い版で同じファイルを公開していたなら、そのファイルのファイルごとの比較を残す (Issue #314 の
/// 4 回目のレビュー)。
#[test]
fn files_reached_by_other_crates_in_either_version_keep_file_reports() {
    let workspace = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
    let member = |name: &str, lib: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{lib}\n[features]\nx = []\n"
        )
    };
    let (a_manifest, b_manifest) = (member("a", ""), member("b", ""));
    let shared_changed = (
        "a/src/shared.rs",
        Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
    );
    // `include!` で読む
    let api = api_changes_with(
        workspace,
        &[
            ("a/Cargo.toml", &a_manifest),
            ("b/Cargo.toml", &b_manifest),
            ("a/src/lib.rs", "#[path = \"shared.rs\"]\nmod hidden;\n"),
            ("b/src/lib.rs", "include!(\"../../a/src/shared.rs\");\n"),
            ("a/src/shared.rs", "pub fn make() {}\n"),
            (
                "b/tests/use_make.rs",
                "#[test]\nfn uses() {\n    b::make();\n}\n",
            ),
        ],
        &[("a/src/lib.rs", Some("")), shared_changed],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    // 旧い版の lib の root が同じファイルを公開していた
    let api = api_changes_with(
        workspace,
        &[
            ("a/Cargo.toml", &a_manifest),
            (
                "b/Cargo.toml",
                &member("b", "\n[lib]\npath = \"src/old.rs\"\n"),
            ),
            ("a/src/lib.rs", ""),
            (
                "b/src/old.rs",
                "#[path = \"../../a/src/shared.rs\"]\npub mod api;\n",
            ),
            ("b/src/new.rs", ""),
            ("a/src/shared.rs", "pub fn make() {}\n"),
            (
                "b/tests/use_make.rs",
                "#[test]\nfn uses() {\n    b::api::make();\n}\n",
            ),
        ],
        &[
            (
                "a/src/lib.rs",
                Some("#[path = \"shared.rs\"]\nmod hidden;\n"),
            ),
            (
                "b/Cargo.toml",
                Some(&member("b", "\n[lib]\npath = \"src/new.rs\"\n")),
            ),
            shared_changed,
        ],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// module の宣言を消した crate が、変えていない module で同じファイルを `include!` で読み続けるなら、その
/// 置き場の分としてファイルごとの比較を残す (Issue #314 の 4 回目のレビュー)。
#[test]
fn files_read_by_macros_of_the_compared_crate_keep_file_reports() {
    let compat = "pub mod compat {\n    include!(\"api.rs\");\n}\n";
    let api = api_changes(
        &[
            ("src/lib.rs", &format!("pub mod api;\n{compat}")),
            ("src/api.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::compat::make();\n}\n",
            ),
        ],
        &[
            ("src/lib.rs", Some(compat)),
            (
                "src/api.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    let conditions = removal_conditions(&api, "make");
    assert!(conditions.contains(&cfg(&[&[]], &[])), "{api:?}");
    assert!(conditions.contains(&cfg(&[&[]], &[&[X]])), "{api:?}");
}

/// module の宣言を消したファイルを、マクロの中の `mod` の宣言が別の置き場に置くなら、ファイルごとの比較を
/// 残す (Issue #314 の 5 回目のレビュー)。
#[test]
fn files_declared_by_macros_keep_file_reports() {
    let compat = "pub mod compat {\n    macro_rules! expose {\n        () => {\n            pub mod shared;\n        };\n    }\n    expose!();\n}\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                &format!("#[path = \"compat/shared.rs\"]\nmod hidden;\n\n{compat}"),
            ),
            ("src/compat/shared.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::compat::shared::make();\n}\n",
            ),
        ],
        &[
            ("src/lib.rs", Some(compat)),
            (
                "src/compat/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// 消した module と同じ名前の定義に属性マクロが付いていれば、展開で module になり得るので、配下の項目を
/// 削除にせず、探し切れなかったことを申告する (Issue #314 の 5 回目のレビュー)。
#[test]
fn removed_module_names_replaced_by_attribute_macros_block() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::api::make();\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[macros::preserve_api]\npub enum api {}\n"),
        )],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
}

/// マクロに渡す文字列のリテラルがエスケープでファイルの名前を書いても、そのファイルを読み得るとして、
/// ファイルごとの比較を残す (Issue #314 の 6 回目のレビュー)。
#[test]
fn files_named_by_escaped_macro_strings_keep_file_reports() {
    let compat = "pub mod compat {\n    macro_rules! expose {\n        ($file:literal) => {\n            #[path = $file]\n            pub mod api;\n        };\n    }\n    expose!(\"\\x73hared.rs\");\n}\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                &format!("#[path = \"compat/shared.rs\"]\nmod hidden;\n\n{compat}"),
            ),
            ("src/compat/shared.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::compat::api::make();\n}\n",
            ),
        ],
        &[
            ("src/lib.rs", Some(compat)),
            (
                "src/compat/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// 消した module の親の inline module に属性マクロが付いていれば、展開で同じ名前の module が現れ得るので、
/// 配下の項目を削除にせず、探し切れなかったことを申告する (Issue #314 の 6 回目のレビュー)。
#[test]
fn removed_modules_under_attribute_macro_parents_block() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer {\n    pub mod api;\n}\n"),
            ("src/outer/api.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::outer::api::make();\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[macros::keep_api]\npub mod outer {}\n"),
        )],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
}

/// 消した module の祖先の inline module に属性マクロが付いていれば、間に別のファイルの module を挟んでも、
/// 展開で同じ名前の module が現れ得るので、探し切れなかったことを申告する (Issue #314 の 7 回目のレビュー)。
#[test]
fn removed_modules_under_attribute_macro_ancestors_block() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer {\n    pub mod middle;\n}\n"),
            ("src/outer/middle.rs", "pub mod api;\n"),
            ("src/outer/middle/api.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::outer::middle::api::make();\n}\n",
            ),
        ],
        &[
            (
                "src/lib.rs",
                Some("#[macros::keep_api]\npub mod outer {\n    pub mod middle;\n}\n"),
            ),
            ("src/outer/middle.rs", Some("")),
        ],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
}

/// item 位置のマクロは別名の `include!` でも、字面に無いパスでもファイルを読み得るので、マクロを呼び出す
/// crate ではファイルごとの比較を残す (Issue #314 の 7 回目のレビュー)。
#[test]
fn files_read_by_aliased_include_keep_file_reports() {
    let compat = "pub mod compat {\n    use std::include as read;\n    read!(concat!(\n        env!(\"CARGO_MANIFEST_DIR\"),\n        \"/src/sh\",\n        \"ared.rs\"\n    ));\n}\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                &format!("#[path = \"shared.rs\"]\nmod hidden;\n\n{compat}"),
            ),
            ("src/shared.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::compat::make();\n}\n",
            ),
        ],
        &[
            ("src/lib.rs", Some(compat)),
            (
                "src/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// 消した module の親に、属性マクロ・`derive` の付いた項目や item 位置のマクロがあれば、展開が同じ名前の
/// module を作り得るので、探し切れなかったことを申告する。旧新で同じ字面でも、展開 (手続きマクロの実装や
/// `#[cfg]` の別の分岐) が同じとは言えない (Issue #314 の 8・9 回目のレビュー)。名前を宣言した版の module か
/// 祖先に属性マクロが付いていても、その版の配下の項目が展開で変わり得るので申告する。
#[test]
fn removed_module_names_near_attribute_macros() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    let uses = (
        "tests/use_make.rs",
        "#[test]\nfn uses() {\n    notes::api::make();\n}\n",
    );
    // 兄弟の項目に付けた属性マクロ
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make, uses],
        &[("src/lib.rs", Some("#[macros::keep_api]\nstruct Marker;\n"))],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
    // 同じ名前の値の定義と、item 位置のマクロ
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), make, uses],
        &[(
            "src/lib.rs",
            Some("pub fn api() {}\n\nmacro_rules! nothing {\n    () => {};\n}\n\nnothing!();\n"),
        )],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
    // 旧新で同じ字面の `derive`
    let config = "#[derive(Debug)]\npub struct Config;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", &format!("pub mod api;\n{config}")),
            make,
            uses,
        ],
        &[("src/lib.rs", Some(config))],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
    // 名前を宣言した旧い版の `mod` に付いた属性マクロ
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub mod outer {\n    #[macros::strip]\n    pub mod api;\n}\n",
            ),
            ("src/outer/api.rs", "pub fn make() {}\n"),
            (
                "tests/use_make.rs",
                "#[test]\nfn uses() {\n    notes::outer::api::make();\n}\n",
            ),
        ],
        &[("src/lib.rs", Some("pub mod outer {}\n"))],
    );
    assert_eq!(reports(&api, "make").0, 0, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
}

/// 比べ直していない crate の属性マクロもファイルを読み得るので、ファイルごとの比較を残す (Issue #314 の
/// 8 回目のレビュー)。
#[test]
fn files_read_by_attribute_macros_of_other_crates_keep_file_reports() {
    let member = |name: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n"
        )
    };
    let api = api_changes_with(
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        &[
            ("a/Cargo.toml", &member("a")),
            ("b/Cargo.toml", &member("b")),
            ("a/src/lib.rs", "#[path = \"shared.rs\"]\nmod hidden;\n"),
            (
                "b/src/lib.rs",
                "#[macros::load_shared]\npub mod compat {}\n",
            ),
            ("a/src/shared.rs", "pub fn make() {}\n"),
            (
                "b/tests/use_make.rs",
                "#[test]\nfn uses() {\n    b::compat::make();\n}\n",
            ),
        ],
        &[
            ("a/src/lib.rs", Some("")),
            (
                "a/src/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
}

/// 消した module の中の属性マクロ・`derive` が作る項目は数えられないので、見える項目の削除を出したうえで、
/// 探し切れなかったことを申告する。どの設定にも無い項目の属性は数えない (Issue #314 の 10 回目のレビュー)。
#[test]
fn removed_modules_with_attribute_macros_inside_report_and_block() {
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "#[derive(api_macros::Api)]\nstruct Marker;\n\npub fn make() {}\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(incomplete(&api), "{api:?}");
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "#[cfg(any())]\n#[derive(Debug)]\nstruct Marker;\n\npub fn make() {}\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// 消した module の親の、どの設定にも無い項目の属性やマクロの呼び出しは、名前を作らない (Issue #314 の
/// 10 回目のレビュー)。
#[test]
fn never_present_attributes_and_macros_do_not_block_removed_modules() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    for kept in [
        "#[cfg(any())]\n#[derive(Debug)]\nstruct Marker;\n",
        "macro_rules! nothing {\n    () => {};\n}\n\n#[cfg(any())]\nnothing!();\n",
    ] {
        let api = api_changes(
            &[("src/lib.rs", &format!("pub mod api;\n{kept}")), make],
            &[("src/lib.rs", Some(kept))],
        );
        assert_eq!(classify(&api, "make").0, "rm_dead", "{kept} {api:?}");
        assert!(!incomplete(&api), "{kept} {api:?}");
    }
}

/// 消した module の中のマクロの呼び出しと属性の付いた項目は、置き場に着く条件・項目自身の条件と合わせて
/// どこかの設定で有るものだけを数える。`#[cfg]` で分けた同じ名前の inline module の、どの設定でも無い分岐の
/// 属性マクロも数えない (Issue #314 の 11 回目のレビュー)。
#[test]
fn macros_and_attributes_that_never_hold_with_their_places_are_ignored() {
    // どの設定にも無いマクロの呼び出し
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "macro_rules! nothing {\n    () => {};\n}\n\n#[cfg(any())]\nnothing!();\n\npub fn make() {}\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 置き場の条件と両立しない属性の付いた項目
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod api;\n"),
            (
                "src/api.rs",
                "#[cfg(not(feature = \"x\"))]\n#[derive(Debug)]\nstruct Marker;\n\npub fn make() {}\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[X]], &[])),
        "{api:?}"
    );
    assert!(!incomplete(&api), "{api:?}");
    // どの設定にも無い、同じ名前の inline module の分岐の属性マクロ
    let never = "\n#[cfg(any())]\n#[macros::rewrite]\npub mod outer {}\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                &format!("pub mod outer {{\n    pub mod api;\n}}\n{never}"),
            ),
            ("src/outer/api.rs", "pub fn make() {}\n"),
        ],
        &[("src/lib.rs", Some(&format!("pub mod outer {{}}\n{never}")))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// `impl`・trait の中のマクロの呼び出しと属性の付いた関連項目は、ブロックと項目自身の条件で数え、置き換え
/// 得る属性は `cfg_attr` の述語の下で適用されるとし、ファイルの先頭の `#![cfg]` も合わせる (Issue #314 の
/// 12 回目のレビュー)。
#[test]
fn macros_and_attributes_inside_blocks_follow_their_own_conditions() {
    // trait の中のマクロは関連項目を作り得る (main と同じく探し切れない)
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "macro_rules! members {\n    () => {\n        fn generated(&self);\n    };\n}\n\npub trait Api {\n    members!();\n}\n",
            ),
        ],
        &[(
            "src/lib.rs",
            Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
        )],
    );
    assert!(incomplete(&api), "{api:?}");
    // impl の中の、どの設定にも無いマクロの呼び出し
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "macro_rules! nothing {\n    () => {};\n}\n\npub struct Api;\n\nimpl Api {\n    #[cfg(any())]\n    nothing!();\n}\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(reports(&api, "Api").0, 1, "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // impl の関連項目に付いた属性マクロ (見える項目の削除を出したうえで申告する)
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                "pub struct Api;\n\nimpl Api {\n    #[api_macros::expose]\n    fn marker() {}\n}\n",
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(reports(&api, "Api").0, 1, "{api:?}");
    assert!(incomplete(&api), "{api:?}");
    // 決して適用されない `cfg_attr` の属性マクロ
    let make = ("src/api.rs", "pub fn make() {}\n");
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg_attr(any(), macros::rewrite)]\npub mod api;\n",
            ),
            make,
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // ファイルの先頭の `#![cfg]` と両立しない属性の付いた項目
    let kept = "\n#[cfg(not(feature = \"x\"))]\n#[derive(Debug)]\nstruct Marker;\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                &format!("#![cfg(feature = \"x\")]\npub mod api;\n{kept}"),
            ),
            make,
        ],
        &[(
            "src/lib.rs",
            Some(&format!("#![cfg(feature = \"x\")]\n{kept}")),
        )],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// 消した module の名前を作り得ないものでは、探し切れなかったことにしない: trait の中のマクロ (関連項目しか
/// 作らない)、入れ子の `cfg_attr` で決して適用されない属性マクロ、祖先のファイルの先頭の `#![cfg]` と両立
/// しない属性の付いた項目、中身の先頭の `#![cfg]` でどの設定にも無い inline module の属性マクロ (Issue #314
/// の 13 回目のレビュー)。
#[test]
fn things_that_cannot_make_the_removed_name_do_not_block() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    for kept in [
        "macro_rules! members {\n    () => {\n        fn marker(&self);\n    };\n}\n\ntrait Marker {\n    members!();\n}\n",
        "#[macros::rewrite]\nmod disabled {\n    #![cfg(any())]\n}\n",
    ] {
        let api = api_changes(
            &[("src/lib.rs", &format!("pub mod api;\n{kept}")), make],
            &[("src/lib.rs", Some(kept))],
        );
        assert_eq!(classify(&api, "make").0, "rm_dead", "{kept} {api:?}");
        assert!(!incomplete(&api), "{kept} {api:?}");
    }
    // 入れ子の `cfg_attr` で決して適用されない属性マクロ
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg_attr(all(), cfg_attr(any(), macros::rewrite))]\npub mod api;\n",
            ),
            make,
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 祖先のファイルの先頭の `#![cfg]` と両立しない属性の付いた項目
    let marker = "#[cfg(feature = \"x\")]\n#[derive(Debug)]\nstruct Marker;\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#![cfg(not(feature = \"x\"))]\npub mod parent;\n",
            ),
            ("src/parent.rs", &format!("{marker}\npub mod api;\n")),
            ("src/parent/api.rs", "pub fn make() {}\n"),
        ],
        &[("src/parent.rs", Some(marker))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 途中のファイルの先頭の `#![cfg]` と両立しない属性の付いた項目
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod outer;\n"),
            (
                "src/outer.rs",
                "#![cfg(not(feature = \"x\"))]\npub mod parent;\n",
            ),
            ("src/outer/parent.rs", &format!("{marker}\npub mod api;\n")),
            ("src/outer/parent/api.rs", "pub fn make() {}\n"),
        ],
        &[("src/outer/parent.rs", Some(marker))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// 属性マクロは中の `#[cfg]` を解かずに受け取るので、属性マクロに囲まれたマクロの呼び出し (inline module と
/// `impl` ブロックに付いたもの) は、呼び出し自身の条件がどの設定でも成り立たなくても数える (main と同じく
/// 探し切れない。Issue #314 の 14 回目のレビュー)。
#[test]
fn macros_wrapped_by_attribute_macros_are_counted_regardless_of_their_own_conditions() {
    let unused = "macro_rules! unused {\n    () => {};\n}\n\n";
    for wrapped in [
        "#[macros::rewrite]\nmod hidden {\n    #[cfg(any())]\n    unused!();\n}\n",
        "pub struct Api;\n\n#[macros::rewrite]\nimpl Api {\n    #[cfg(any())]\n    unused!();\n}\n",
    ] {
        let api_rs = format!("{unused}{wrapped}\npub fn make() {{}}\n");
        let api = api_changes(
            &[("src/lib.rs", "pub mod api;\n"), ("src/api.rs", &api_rs)],
            &[(
                "src/lib.rs",
                Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
            )],
        );
        assert!(incomplete(&api), "{wrapped} {api:?}");
        // 属性マクロに囲まれていなければ、どの設定にも無い呼び出しは数えない
        let api_rs = api_rs.replace("#[macros::rewrite]\n", "");
        let api = api_changes(
            &[("src/lib.rs", "pub mod api;\n"), ("src/api.rs", &api_rs)],
            &[(
                "src/lib.rs",
                Some("#[cfg(feature = \"x\")]\npub mod api;\n"),
            )],
        );
        assert_eq!(
            classify(&api, "make"),
            ("rm_dead", cfg(&[&[]], &[&[X]])),
            "{wrapped} {api:?}"
        );
        assert!(!incomplete(&api), "{wrapped} {api:?}");
    }
}

/// 項目を置き換えない組み込みの属性 (`link`・`panic_handler`・`no_implicit_prelude`・Rust 2024 の
/// `unsafe(no_mangle)` など。`cfg_attr` の中も) は、消した module の名前や中身を作り得るものに数えない。
/// 関連関数の本体の先頭の `#![cfg]` で、どの設定にも無い関連関数の属性マクロも数えない (Issue #314 の
/// 14・15 回目のレビュー)。
#[test]
fn builtin_attributes_and_absent_associated_functions_do_not_block() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    let panic_handler =
        "#[panic_handler]\nfn panic(_: &core::panic::PanicInfo<'_>) -> ! {\n    loop {}\n}\n";
    // 消した module の親の、`link` の付いた extern ブロック、`panic_handler`、`no_implicit_prelude`
    for kept in [
        "\n#[link(name = \"kernel32\")]\nunsafe extern \"C\" {\n    fn GetTickCount() -> u32;\n}\n"
            .to_owned(),
        format!("\n{panic_handler}"),
        "\n#[no_implicit_prelude]\nmod plain {}\n".to_owned(),
    ] {
        let api = api_changes(
            &[("src/lib.rs", &format!("pub mod api;\n{kept}")), make],
            &[("src/lib.rs", Some(&kept))],
        );
        assert_eq!(classify(&api, "make").0, "rm_dead", "{kept} {api:?}");
        assert!(!incomplete(&api), "{kept} {api:?}");
    }
    // 消した module の中の、`unsafe(..)` で包んだ組み込みの属性と `panic_handler`
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            (
                "src/api.rs",
                &format!("{panic_handler}\npub fn make() {{}}\n"),
            ),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    for attribute in [
        "#[unsafe(no_mangle)]",
        "#[unsafe(export_name = \"exported_symbol\")]",
        "#[cfg_attr(windows, unsafe(no_mangle))]",
    ] {
        let api = api_changes(
            &[
                ("src/lib.rs", "pub mod api;\n"),
                (
                    "src/api.rs",
                    &format!(
                        "{attribute}\npub extern \"C\" fn exported() {{}}\n\npub fn make() {{}}\n"
                    ),
                ),
            ],
            &[("src/lib.rs", Some(""))],
        );
        assert_eq!(classify(&api, "make").0, "rm_dead", "{attribute} {api:?}");
        assert!(!incomplete(&api), "{attribute} {api:?}");
    }
    // 本体の先頭の `#![cfg]` でどの設定にも無い、属性マクロの付いた関連関数 (impl と trait のデフォルト)
    for block in [
        "pub struct Api;\n\nimpl Api {\n    #[api_macros::expose]\n    fn marker() {\n        #![cfg(any())]\n    }\n}\n",
        "pub trait Api {\n    #[api_macros::expose]\n    fn marker() {\n        #![cfg(any())]\n    }\n}\n",
    ] {
        let api = api_changes(
            &[("src/lib.rs", "pub mod api;\n"), ("src/api.rs", block)],
            &[("src/lib.rs", Some(""))],
        );
        assert_eq!(reports(&api, "Api").0, 1, "{block} {api:?}");
        assert!(!incomplete(&api), "{block} {api:?}");
    }
}

/// ファイルごと移した module (`mod api;` を `mod v2;` にし、`api.rs` を `v2.rs` へ移す) は、#314 の前と
/// 同じくファイルの比較に任せる (移したファイルの項目は移した先の module に残る)。移した先が lib crate の
/// module でなければ、配下の項目は消えたものとして出す。
#[test]
fn modules_moved_with_their_files_are_left_to_file_reports() {
    let api_rs = "#[derive(Debug)]\npub struct Marker;\n\npub fn make() {}\n";
    // 同じ crate の module へ移す (移したファイルのマクロの呼び出しも、ファイルの比較に任せる)
    let moved = format!("{api_rs}\nmacro_rules! nothing {{\n    () => {{}};\n}}\n\nnothing!();\n");
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), ("src/api.rs", &moved)],
        &[
            ("src/lib.rs", Some("pub mod v2;\n")),
            ("src/api.rs", None),
            ("src/v2.rs", Some(&moved)),
        ],
    );
    assert_eq!(classify(&api, "make").0, "none", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // `#[path]` で lib が読む、bin のディレクトリの中のファイルへ移す (置かれたディレクトリの package に
    // 限らず、lib の module のファイルになるかで決める)
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub mod api;\n\npub fn run() {\n    api::make();\n}\n",
            ),
            ("src/api.rs", api_rs),
        ],
        &[
            (
                "src/lib.rs",
                Some(
                    "#[path = \"bin/support/api.rs\"]\npub mod v2;\n\npub fn run() {\n    v2::make();\n}\n",
                ),
            ),
            ("src/api.rs", None),
            ("src/bin/support/api.rs", Some(api_rs)),
        ],
    );
    assert_eq!(classify(&api, "make").0, "none", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // bin の module へ移す (lib の公開の項目からは消える)
    let api = api_changes(
        &[("src/lib.rs", "pub mod api;\n"), ("src/api.rs", api_rs)],
        &[
            ("src/lib.rs", Some("")),
            ("src/api.rs", None),
            ("src/bin/tool/api.rs", Some(api_rs)),
            ("src/bin/tool/main.rs", Some("mod api;\n\nfn main() {}\n")),
        ],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(incomplete(&api), "{api:?}");
}

/// 片方の版にしか無い置き場 (消した module) があるファイルは、旧新の両方で比べた置き場があっても、
/// ファイルごとの比較 (宣言自身の条件の変化) を残す (比べ直していない crate も同じファイルを置き得る)。
/// 片方の版にしか無い項目の一部の宣言だけをファイルの中身の変化が説明するときは、残りの宣言の置き場の
/// 有無を比べる (Issue #314 の 16 回目のレビュー)。
#[test]
fn one_sided_placements_keep_file_reports_and_unexplained_declarations() {
    let manifest = |name: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n"
        )
    };
    // 消した module と、条件の変わった module が同じファイルを置く。比べ直していない b もそのファイルを公開する
    let (a, b) = (manifest("a"), manifest("b"));
    let api = api_changes_with(
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        &[
            ("a/Cargo.toml", &a),
            ("b/Cargo.toml", &b),
            (
                "a/src/lib.rs",
                "#[path = \"shared.rs\"]\nmod removed;\n#[cfg(feature = \"x\")]\n#[path = \"shared.rs\"]\nmod kept;\n",
            ),
            (
                "b/src/lib.rs",
                "#[path = \"../../a/src/shared.rs\"]\npub mod api;\n",
            ),
            ("a/src/shared.rs", "pub fn make() {}\n"),
            (
                "b/tests/use_make.rs",
                "#[test]\nfn uses() {\n    b::api::make();\n}\n",
            ),
        ],
        &[
            ("a/src/lib.rs", Some("#[path = \"shared.rs\"]\nmod kept;\n")),
            (
                "a/src/shared.rs",
                Some("#[cfg(feature = \"x\")]\npub fn make() {}\n"),
            ),
        ],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[]], &[&[X]])),
        "{api:?}"
    );
    // 消した module の非公開の分岐のファイルだけを変えても、変えていない公開の分岐の削除は出す
    let (a, tool) = (
        manifest("a"),
        "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2024\"\n".to_owned(),
    );
    let api = api_changes_with(
        "[workspace]\nmembers = [\"a\", \"tool\"]\nresolver = \"2\"\n",
        &[
            ("a/Cargo.toml", &a),
            ("tool/Cargo.toml", &tool),
            (
                "a/src/lib.rs",
                "#[cfg(feature = \"x\")]\npub mod api;\n#[cfg(not(feature = \"x\"))]\n#[path = \"../../tool/src/helper.rs\"]\nmod api;\n",
            ),
            ("a/src/api.rs", "pub fn make() {}\n"),
            ("tool/src/helper.rs", "pub fn make() {}\n"),
            ("tool/src/main.rs", "mod helper;\n\nfn main() {}\n"),
            (
                "a/tests/use_api.rs",
                "#[cfg(feature = \"x\")]\n#[test]\nfn uses() {\n    a::api::make();\n}\n",
            ),
        ],
        &[("a/src/lib.rs", Some("")), ("tool/src/helper.rs", Some(""))],
    );
    assert!(
        removal_conditions(&api, "make").contains(&cfg(&[&[X]], &[])),
        "{api:?}"
    );
}

/// どの設定にも無い `mod` の宣言 (中身のファイルが無くてもよい) は、消した module の名前や経路を決めるときに
/// 開かない。ファイルごとの比較が公開 API 面の外として出さない削除 (bin だけの package に置いたファイル) は、
/// ファイルごとの比較に任せず、lib が公開していた置き場の削除として出す (Issue #314 の 17 回目のレビュー)。
#[test]
fn never_present_declarations_and_unreported_file_removals_do_not_hide_removals() {
    // どの設定にも無い、同じ名前の `mod` の宣言 (ファイルが無い)
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub mod api;\n\n#[cfg(any())]\n#[path = \"missing.rs\"]\nmod api;\n",
            ),
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[("src/lib.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 囲む module (別のファイル) の条件と両立しない `mod` の宣言 (ファイルが無い)
    let api = api_changes(
        &[
            ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod outer;\n"),
            (
                "src/outer.rs",
                "pub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"missing.rs\"]\npub mod api;\n",
            ),
            ("src/outer/api.rs", "pub fn make() {}\n"),
        ],
        &[("src/outer.rs", Some(""))],
    );
    assert_eq!(
        classify(&api, "make"),
        ("rm_dead", cfg(&[&[X]], &[])),
        "{api:?}"
    );
    assert!(!incomplete(&api), "{api:?}");
    // lib が `#[path]` で公開する、bin だけの package のファイル (中身を消す・ファイルを消す)
    let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    let tool = "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    for (helper, main) in [
        (Some(""), "mod helper;\n\nfn main() {}\n"),
        (None, "fn main() {}\n"),
    ] {
        let api = api_changes_with(
            "[workspace]\nmembers = [\"a\", \"tool\"]\nresolver = \"2\"\n",
            &[
                ("a/Cargo.toml", a),
                ("tool/Cargo.toml", tool),
                (
                    "a/src/lib.rs",
                    "#[path = \"../../tool/src/helper.rs\"]\npub mod api;\n",
                ),
                ("tool/src/main.rs", "mod helper;\n\nfn main() {}\n"),
                ("tool/src/helper.rs", "pub fn make() {}\n"),
            ],
            &[
                ("a/src/lib.rs", Some("")),
                ("tool/src/helper.rs", helper),
                ("tool/src/main.rs", Some(main)),
            ],
        );
        assert_eq!(classify(&api, "make").0, "rm_dead", "{helper:?} {api:?}");
    }
}

/// ファイルの先頭の `#![cfg]` や、glob・取り込みの先へたどる条件と合わせてどの設定にも無い `mod` の宣言は
/// 開かない。消したファイルの項目をファイルごとの比較が公開 API 面の外として出さない (別の lib の非公開の
/// module のファイル) なら、ファイルごとの比較に任せない (Issue #314 の 18 回目のレビュー)。
#[test]
fn declarations_unreachable_through_files_and_globs_are_not_opened() {
    let make = ("src/api.rs", "pub fn make() {}\n");
    let missing = "#[cfg(not(feature = \"x\"))]\n#[path = \"missing.rs\"]\n";
    // crate root の先頭の `#![cfg]` と両立しない宣言
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                &format!("#![cfg(feature = \"x\")]\npub mod api;\n\n{missing}mod api;\n"),
            ),
            make,
        ],
        &[("src/lib.rs", Some("#![cfg(feature = \"x\")]\n"))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // glob で取り込む module の中の、glob の条件と両立しない宣言
    let rest =
        "\n#[cfg(feature = \"x\")]\nmod outer;\n#[cfg(feature = \"x\")]\nuse crate::outer::*;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", &format!("pub mod api;\n{rest}")),
            ("src/outer.rs", &format!("{missing}pub mod api;\n")),
            make,
        ],
        &[("src/lib.rs", Some(rest))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 別の lib の非公開の module のファイルを、lib が `#[path]` で公開していて、そのファイルを消す
    let manifest = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
    };
    let (a, b) = (manifest("a"), manifest("b"));
    let api = api_changes_with(
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        &[
            ("a/Cargo.toml", &a),
            ("b/Cargo.toml", &b),
            (
                "a/src/lib.rs",
                "#[path = \"../../b/src/helper.rs\"]\npub mod api;\n",
            ),
            ("b/src/lib.rs", "mod helper;\n"),
            ("b/src/helper.rs", "pub fn make() {}\n"),
        ],
        &[
            ("a/src/lib.rs", Some("")),
            ("b/src/lib.rs", Some("")),
            ("b/src/helper.rs", None),
        ],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
}

/// 属性マクロは中の `#[cfg]` を解く前の inline module を受け取るので、名前解決で inline module を `#[cfg]` で
/// 除かない (main と同じく契約が名指す型別名の右辺の変化を出す)。crate root の先頭の `#![cfg]` は、別の
/// ファイルへたどった先の宣言にも効く。消した module と同じ名前の、どの設定にも無い型別名の右辺は解かない
/// (Issue #314 の 19 回目のレビュー)。
#[test]
fn wrapped_inline_modules_root_conditions_and_unused_aliases() {
    // 属性マクロに囲まれた、どの設定にも無い inline module の中の型別名
    let wrapped = |text: &str| {
        format!(
            "#[macros::enable]\nmod hidden {{\n    #[cfg(any())]\n    pub mod types {{\n        pub(crate) type Text = {text};\n    }}\n}}\n\npub fn run(_: hidden::types::Text) {{}}\n"
        )
    };
    let api = api_changes(
        &[("src/lib.rs", &wrapped("u8"))],
        &[("src/lib.rs", Some(&wrapped("u16")))],
    );
    assert!(
        api.modified.iter().any(|change| change.name == "run"),
        "{api:?}"
    );
    // crate root の `#![cfg]` と両立しない、別のファイルの中の宣言 (ファイルが無い)。新しい版でその宣言も
    // 消すと、消した module は新しい版に経路が無くなり、旧い版の経路をたどる
    let selected = "#[cfg(feature = \"x\")]\n#[path = \"real.rs\"]\npub mod selected;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", "#![cfg(feature = \"x\")]\npub mod outer;\n"),
            (
                "src/outer.rs",
                &format!(
                    "{selected}\n#[cfg(not(feature = \"x\"))]\n#[path = \"missing.rs\"]\npub mod selected;\n"
                ),
            ),
            ("src/real.rs", "#[path = \"api.rs\"]\npub mod api;\n"),
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[("src/outer.rs", Some(selected)), ("src/real.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
    // 消した module と同じ名前の、どの設定にも無い型別名 (右辺の経路を決められない)
    let rest = "\n#[cfg(any())]\ntype api = detail::Thing;\n\n#[cfg_attr(feature = \"x\", path = \"alternate.rs\")]\nmod detail;\n";
    let api = api_changes(
        &[
            ("src/lib.rs", &format!("pub mod api;\n{rest}")),
            ("src/api.rs", "pub fn make() {}\n"),
            ("src/detail.rs", "pub struct Thing;\n"),
            ("src/alternate.rs", "pub struct Thing;\n"),
        ],
        &[("src/lib.rs", Some(rest))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// 経路で入ったファイルの先頭の `#![cfg]` は、名前を作り得るものの到達の判定にだけ使い、契約が名指す束縛の
/// 識別には入れない (常に成り立つ `#![cfg(all())]` を足しただけで束縛が変わったことにしない。Issue #314 の
/// 20 回目のレビュー)。
#[test]
fn entered_file_conditions_do_not_change_binding_identities() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod hidden;\n\npub fn run(_: hidden::Text) {}\n",
            ),
            ("src/hidden.rs", "pub type Text = u8;\n"),
        ],
        &[(
            "src/hidden.rs",
            Some("#![cfg(all())]\npub(crate) type Text = u8;\n"),
        )],
    );
    assert!(
        api.modified.iter().all(|change| change.name != "run"),
        "{api:?}"
    );
}

/// どの設定にも無い宣言は、中身のファイルを開けないときだけ飛ばし、開けた宣言の候補は変えない (属性マクロの
/// 中の `use` の展開前の `#[cfg]` で、契約が名指す束縛を変えない)。module でない枝 (マクロが作り得る名前) も、
/// crate root の先頭の `#![cfg]` と合わせて判定する (Issue #314 の 21 回目のレビュー)。
#[test]
fn unopened_declarations_do_not_change_resolved_bindings() {
    // 属性マクロの中の、どの設定にも無い `use` を通る契約の束縛
    let lib = "mod hidden;\n\n#[rewriter::enable_uses]\npub mod api {\n    #[cfg(any())]\n    use crate::hidden as types;\n\n    pub fn run(_: types::Text) {}\n}\n";
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/hidden.rs", "pub type Text = u8;\n"),
        ],
        &[("src/hidden.rs", Some("pub type Text = u16;\n"))],
    );
    assert!(
        api.modified.iter().any(|change| change.name == "run"),
        "{api:?}"
    );
    // crate root の `#![cfg]` と両立しない `use` が持ち込む、マクロのある module の名前
    let lib = "#![cfg(feature = \"x\")]\n\n#[path = \"real.rs\"]\npub mod selected;\n\n#[cfg(not(feature = \"x\"))]\nuse crate::noise::ghost as selected;\n\nmod noise {\n    macro_rules! nothing {\n        () => {};\n    }\n    nothing!();\n}\n";
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/real.rs", "#[path = \"api.rs\"]\npub mod api;\n"),
            ("src/api.rs", "pub fn make() {}\n"),
        ],
        &[("src/real.rs", Some(""))],
    );
    assert_eq!(classify(&api, "make").0, "rm_dead", "{api:?}");
    assert!(!incomplete(&api), "{api:?}");
}

/// 開けない宣言を飛ばすのは、消した module の名前を作り得るものを探す間だけにし、契約が名指す束縛の比較
/// (#312) では、属性マクロの中の `use` の展開前の `#[cfg]` で決めずに、今までどおり解けないこととして
/// 出す (Issue #314 の 22 回目のレビュー)。
#[test]
fn contract_comparisons_keep_unresolved_wrapped_imports() {
    let lib = "#[cfg_attr(feature = \"x\", path = \"alternate.rs\")]\nmod hidden;\n\n#[rewriter::enable_uses]\npub mod api {\n    #[cfg(any())]\n    use crate::hidden as types;\n\n    pub fn run(_: types::Text) {}\n}\n";
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/hidden.rs", "pub(crate) type Text = u8;\n"),
            ("src/alternate.rs", "pub(crate) type Text = u8;\n"),
        ],
        &[("src/hidden.rs", Some("pub(crate) type Text = u16;\n"))],
    );
    assert!(
        api.modified.iter().any(|change| change.name == "run"),
        "{api:?}"
    );
}
