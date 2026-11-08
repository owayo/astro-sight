//! Rust の公開の項目の有る設定 (項目自身と囲む項目の `#[cfg]`) の変更 (Issue #291)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

use crate::models::review::{ApiChanges, ApiModificationUnverifiedReason, CfgAvailability};

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\ny = []\n";

/// `files` をコミットし、`changes` に書き換えたときの API の差分。
fn api_changes(files: &[(&str, &str)], changes: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut all = vec![("Cargo.toml", CARGO)];
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

/// `src/lib.rs` を `old` から `new` に変えたときの `name` の区分 (`extra` は変えないほかのファイル)。
fn bucket(old: &str, new: &str, extra: &[(&str, &str)], name: &str) -> &'static str {
    let mut files = vec![("src/lib.rs", old)];
    files.extend_from_slice(extra);
    classify(&api_changes(&files, &[("src/lib.rs", new)]), name).0
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

/// Issue の再現: 公開の関数・構造体・enum に `#[cfg]` を足すと、足さなかった設定から消える。
/// 削除と同じく、参照が残らなければ removed_dead、残れば removed (blocking)。
#[test]
fn rust_item_cfg_added_is_a_removal() {
    let old = "pub fn make() -> u32 {\n    1\n}\n\npub struct Config {\n    pub name: u32,\n}\n\npub enum Kind {\n    A,\n}\n";
    let new = "#[cfg(feature = \"x\")]\npub fn make() -> u32 {\n    1\n}\n\n#[cfg(feature = \"x\")]\npub struct Config {\n    pub name: u32,\n}\n\n#[cfg(feature = \"x\")]\npub enum Kind {\n    A,\n}\n";
    let api = api_changes(&[("src/lib.rs", old)], &[("src/lib.rs", new)]);
    for name in ["make", "Config", "Kind"] {
        assert_eq!(
            classify(&api, name),
            ("rm_dead", cfg(&[&[]], &[&["cfg(feature = \"x\")"]])),
            "{name}"
        );
    }
    assert!(api.modified.is_empty(), "{:?}", api.modified);
    // 参照が残れば blocking な removed
    let user = "pub fn other() -> u32 {\n    crate::make()\n}\n";
    let lib = |attribute: &str| {
        format!("pub mod user;\n\n{attribute}pub fn make() -> u32 {{\n    1\n}}\n")
    };
    assert_eq!(
        bucket(
            &lib(""),
            &lib("#[cfg(feature = \"x\")]\n"),
            &[("src/user.rs", user)],
            "make"
        ),
        "rm"
    );
}

/// 条件を狭める・別の条件に付け替えるのは消える設定、広げる・外すのは増える設定。同値な書き換えと
/// 分岐の分割・まとめは変化にしない。
#[test]
fn rust_item_cfg_changes_follow_cfg_semantics() {
    let make = |attributes: &str| format!("{attributes}pub fn make() -> u32 {{\n    1\n}}\n");
    let cases: &[(&str, &str, &str)] = &[
        // 狭める (`unix` から Linux だけへ)
        (
            "#[cfg(unix)]\n",
            "#[cfg(target_os = \"linux\")]\n",
            "rm_dead",
        ),
        // 広げる (Linux から `unix` へ。`target_os = "linux"` は `unix` を含む) と、外す
        ("#[cfg(target_os = \"linux\")]\n", "#[cfg(unix)]\n", "add"),
        ("#[cfg(feature = \"x\")]\n", "", "add"),
        // 別の条件への付け替え (消える設定がある)
        (
            "#[cfg(feature = \"x\")]\n",
            "#[cfg(feature = \"y\")]\n",
            "rm_dead",
        ),
        // 同値な書き換え
        (
            "#[cfg(all(feature = \"x\", feature = \"y\"))]\n",
            "#[cfg(all(feature = \"y\", feature = \"x\"))]\n",
            "none",
        ),
        (
            "#[cfg(feature = \"x\")]\n#[cfg(feature = \"y\")]\n",
            "#[cfg(all(feature = \"x\", feature = \"y\"))]\n",
            "none",
        ),
        (
            "#[cfg(target_family = \"unix\")]\n",
            "#[cfg(unix)]\n",
            "none",
        ),
        // `cfg_attr` の中の `cfg` 以外の属性は有る設定に効かない
        (
            "#[cfg_attr(feature = \"x\", cfg(feature = \"y\"))]\n",
            "#[cfg_attr(feature = \"x\", cfg(feature = \"y\"), inline)]\n",
            "none",
        ),
        // `cfg_attr(P, cfg(Q))` は `not(P) or Q`
        (
            "#[cfg_attr(feature = \"x\", cfg(feature = \"y\"))]\n",
            "#[cfg(any(not(feature = \"x\"), feature = \"y\"))]\n",
            "none",
        ),
    ];
    for (old, new, expected) in cases {
        assert_eq!(
            bucket(&make(old), &make(new), &[], "make"),
            *expected,
            "{old} => {new}"
        );
    }
    // 増える設定は条件を添えた追加
    let api = api_changes(
        &[("src/lib.rs", &make("#[cfg(feature = \"x\")]\n"))],
        &[("src/lib.rs", &make(""))],
    );
    assert_eq!(
        classify(&api, "make"),
        ("add", cfg(&[&["cfg(feature = \"x\")"]], &[&[]]))
    );
    // 本体の内側の `#![cfg]` も関数の有る条件
    assert_eq!(
        bucket(
            "pub fn make() -> u32 {\n    1\n}\n",
            "pub fn make() -> u32 {\n    #![cfg(feature = \"x\")]\n    1\n}\n",
            &[],
            "make"
        ),
        "rm_dead"
    );
    // 非公開の項目は報告しない
    assert_eq!(
        bucket(
            "pub fn other() {}\nfn make() -> u32 {\n    1\n}\n",
            "pub fn other() {}\n#[cfg(feature = \"x\")]\nfn make() -> u32 {\n    1\n}\n",
            &[],
            "make"
        ),
        "none"
    );
}

/// 同じ場所の `#[cfg]` の分岐は、分岐の条件の OR で比べる。
#[test]
fn rust_item_cfg_branches_are_compared_together() {
    let unconditional = "pub fn run() -> u32 {\n    1\n}\n";
    let split = "#[cfg(unix)]\npub fn run() -> u32 {\n    1\n}\n\n#[cfg(not(unix))]\npub fn run() -> u32 {\n    2\n}\n";
    let both = "#[cfg(unix)]\npub fn run() -> u32 {\n    1\n}\n\n#[cfg(windows)]\npub fn run() -> u32 {\n    2\n}\n";
    let unix_only = "#[cfg(unix)]\npub fn run() -> u32 {\n    1\n}\n";
    // 分岐に分けても、まとめても、有る設定は同じ
    assert_eq!(bucket(unconditional, split, &[], "run"), "none");
    assert_eq!(bucket(split, unconditional, &[], "run"), "none");
    // 分岐の片方を消すと、その設定から消える
    assert_eq!(bucket(both, unix_only, &[], "run"), "rm_dead");
    let api = api_changes(&[("src/lib.rs", both)], &[("src/lib.rs", unix_only)]);
    assert_eq!(
        classify(&api, "run").1,
        cfg(&[&["cfg(unix)"], &["cfg(windows)"]], &[&["cfg(unix)"]])
    );
}

/// 囲む項目 (impl・inline module・trait) とファイルの先頭の条件も有る条件。メソッドと関連の項目も
/// 比べる。
#[test]
fn rust_item_cfg_follows_enclosing_items() {
    let cases: &[(&str, &str, &str)] = &[
        // メソッド自身
        (
            "pub struct S;\nimpl S {\n    pub fn f(&self) {}\n}\n",
            "pub struct S;\nimpl S {\n    #[cfg(feature = \"x\")]\n    pub fn f(&self) {}\n}\n",
            "S.f",
        ),
        // impl の条件
        (
            "pub struct S;\nimpl S {\n    pub fn f(&self) {}\n}\n",
            "pub struct S;\n#[cfg(feature = \"x\")]\nimpl S {\n    pub fn f(&self) {}\n}\n",
            "S.f",
        ),
        // inline module の条件 (外側と中身の内側)
        (
            "pub mod m {\n    pub fn f() {}\n}\n",
            "#[cfg(feature = \"x\")]\npub mod m {\n    pub fn f() {}\n}\n",
            "f",
        ),
        (
            "pub mod m {\n    pub fn f() {}\n}\n",
            "pub mod m {\n    #![cfg(feature = \"x\")]\n    pub fn f() {}\n}\n",
            "f",
        ),
        // trait・定数・型別名
        (
            "pub trait T {}\n",
            "#[cfg(feature = \"x\")]\npub trait T {}\n",
            "T",
        ),
        (
            "pub const C: u32 = 1;\n",
            "#[cfg(feature = \"x\")]\npub const C: u32 = 1;\n",
            "C",
        ),
        (
            "pub type Alias = u32;\n",
            "#[cfg(feature = \"x\")]\npub type Alias = u32;\n",
            "Alias",
        ),
    ];
    for (old, new, name) in cases {
        assert_eq!(bucket(old, new, &[], name), "rm_dead", "{old} => {new}");
    }
    // ファイルの先頭の `#![cfg]` (module の中身の全ての項目)
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", "pub fn f() {}\n"),
        ],
        &[("src/api.rs", "#![cfg(feature = \"x\")]\n\npub fn f() {}\n")],
    );
    assert_eq!(
        classify(&api, "f"),
        ("rm_dead", cfg(&[&[]], &[&["cfg(feature = \"x\")"]]))
    );
}

/// 原子が多すぎて比べられない条件の変更は blocking な変更 (理由
/// `rust_cfg_availability_unverified`)。同じ条件のままなら原子の数に関わらず変化にしない。
#[test]
fn rust_item_cfg_without_enough_atoms_stays_blocking() {
    let condition = |count: usize| {
        let atoms: Vec<String> = (0..count).map(|index| format!("a{index}")).collect();
        format!("#[cfg(all({}))]\n", atoms.join(", "))
    };
    let make = |attributes: &str, body: u32| {
        format!("{attributes}pub fn make() -> u32 {{\n    {body}\n}}\n")
    };
    let api = api_changes(
        &[("src/lib.rs", &make(&condition(17), 1))],
        &[("src/lib.rs", &make(&condition(18), 1))],
    );
    let change = api
        .modified
        .iter()
        .find(|change| change.name == "make")
        .expect("make is modified");
    assert_eq!(
        change
            .unverified
            .as_ref()
            .map(|unverified| unverified.reason),
        Some(ApiModificationUnverifiedReason::RustCfgAvailabilityUnverified)
    );
    assert_eq!(
        bucket(
            &make(&condition(20), 1),
            &make(&condition(20), 2),
            &[],
            "make"
        ),
        "none"
    );
}

/// enum・union の形は、旧新の両方に有る設定で、囲む項目の有る条件も合わせて比べる。消える設定は
/// 削除と同じ区分で出すので、形の変更として二重に出さない。
#[test]
fn rust_item_cfg_shapes_are_compared_where_both_exist() {
    // `#[cfg]` を足し、両方に有る設定では同じ値の書き換え (`0` と `0x0`) だけ
    let api = api_changes(
        &[("src/lib.rs", "pub enum Kind {\n    A = 0,\n}\n")],
        &[(
            "src/lib.rs",
            "#[cfg(feature = \"x\")]\npub enum Kind {\n    A = 0x0,\n}\n",
        )],
    );
    assert_eq!(
        classify(&api, "Kind"),
        ("rm_dead", cfg(&[&[]], &[&["cfg(feature = \"x\")"]]))
    );
    assert!(api.modified.is_empty(), "{:?}", api.modified);
    // 両方に有る設定での union のフィールドの追加は互換のまま
    let api = api_changes(
        &[("src/lib.rs", "pub union U {\n    pub a: u32,\n}\n")],
        &[(
            "src/lib.rs",
            "#[cfg(feature = \"x\")]\npub union U {\n    pub a: u32,\n    pub b: u8,\n}\n",
        )],
    );
    assert_eq!(classify(&api, "U").0, "rm_dead");
    assert!(api.modified.iter().all(|change| change.name != "U"));
    assert!(
        api.compatible_modified
            .iter()
            .any(|change| change.name == "U"),
        "{:?}",
        api.compatible_modified
    );
    // 囲む module の条件の下では、同じ条件を variant に書き足しても変化にしない
    assert_eq!(
        bucket(
            "#[cfg(feature = \"x\")]\npub mod m {\n    pub enum Kind {\n        A,\n        B,\n    }\n}\n",
            "#[cfg(feature = \"x\")]\npub mod m {\n    pub enum Kind {\n        A,\n        #[cfg(feature = \"x\")]\n        B,\n    }\n}\n",
            &[],
            "Kind"
        ),
        "none"
    );
    // 両方に有る設定の形の変更は今までどおり
    assert_eq!(
        bucket(
            "#[cfg(feature = \"x\")]\npub enum Kind {\n    A,\n}\n",
            "#[cfg(feature = \"x\")]\npub enum Kind {\n    A,\n    B,\n}\n",
            &[],
            "Kind"
        ),
        "mod"
    );
}

/// 別のファイルへ移した項目も、移動元と移動先の宣言 (分岐をまとめて) の有る設定を比べる。移動は
/// 対応を示すだけで、有る設定を保ったことにはしない。
#[test]
fn rust_item_cfg_is_compared_across_moves() {
    let lib = "pub mod a;\npub mod b;\n";
    let moved = |attributes: &str| {
        api_changes(
            &[
                ("src/lib.rs", lib),
                (
                    "src/a.rs",
                    "pub fn keep() {}\n\n#[cfg(any(feature = \"x\", feature = \"y\"))]\npub fn f() -> u32 {\n    1\n}\n",
                ),
                ("src/b.rs", "pub fn other() {}\n"),
            ],
            &[
                ("src/a.rs", "pub fn keep() {}\n"),
                ("src/b.rs", &format!("pub fn other() {{}}\n\n{attributes}")),
            ],
        )
    };
    let is_moved = |api: &ApiChanges| api.moved.iter().any(|symbol| symbol.name == "f");
    // 条件を狭めて移すと、移動に加えて消える設定を出す
    let api = moved("#[cfg(feature = \"x\")]\npub fn f() -> u32 {\n    1\n}\n");
    assert!(is_moved(&api), "{:?}", api.moved);
    assert_eq!(
        classify(&api, "f"),
        (
            "rm_dead",
            cfg(
                &[&["cfg(any(feature = \"x\", feature = \"y\"))"]],
                &[&["cfg(feature = \"x\")"]]
            )
        )
    );
    // 同じ条件で移すのは移動だけ
    let api =
        moved("#[cfg(any(feature = \"x\", feature = \"y\"))]\npub fn f() -> u32 {\n    1\n}\n");
    assert!(is_moved(&api), "{:?}", api.moved);
    assert_eq!(classify(&api, "f").0, "none");
    // 移動先で同じ範囲の分岐に分けるのも移動だけ
    let api = moved(
        "#[cfg(feature = \"x\")]\npub fn f() -> u32 {\n    1\n}\n\n#[cfg(all(not(feature = \"x\"), feature = \"y\"))]\npub fn f() -> u32 {\n    1\n}\n",
    );
    assert!(is_moved(&api), "{:?}", api.moved);
    assert_eq!(classify(&api, "f").0, "none");
}

/// 同じ場所の `#[cfg]` の分岐: 本体の内側の `#![cfg]` で分けた分岐も条件付きの分岐とし、分岐を足した・
/// 消しただけなら残った分岐の契約は同じ (有る設定の変化だけを出す)。enum・union を分岐に分けたときは
/// 代表の分岐の形しか比べないので、ほかの分岐の設定を飛ばさない。
#[test]
fn rust_item_cfg_branch_changes_are_not_ambiguous() {
    // 本体の内側の `#![cfg]` で同じ範囲に分ける
    assert_eq!(
        bucket(
            "pub fn f() -> u32 {\n    1\n}\n",
            "pub fn f() -> u32 {\n    #![cfg(feature = \"x\")]\n    1\n}\n\npub fn f() -> u32 {\n    #![cfg(not(feature = \"x\"))]\n    2\n}\n",
            &[],
            "f"
        ),
        "none"
    );
    // signature の違う分岐の片方を消すのは、消える設定だけ (組を決められない変更にしない)
    let api = api_changes(
        &[(
            "src/lib.rs",
            "#[cfg(unix)]\npub fn run(x: u8) -> u8 {\n    x\n}\n\n#[cfg(windows)]\npub fn run(x: u16) -> u16 {\n    x\n}\n",
        )],
        &[(
            "src/lib.rs",
            "#[cfg(unix)]\npub fn run(x: u8) -> u8 {\n    x\n}\n",
        )],
    );
    assert_eq!(classify(&api, "run").0, "rm_dead");
    assert!(api.modified.is_empty(), "{:?}", api.modified);
    // enum を分岐に分け、片方の分岐の設定で variant を足すのは壊す
    assert_eq!(
        bucket(
            "pub enum Kind {\n    A,\n}\n",
            "#[cfg(feature = \"x\")]\npub enum Kind {\n    A,\n    #[cfg(not(feature = \"x\"))]\n    B,\n}\n\n#[cfg(not(feature = \"x\"))]\npub enum Kind {\n    A,\n    #[cfg(not(feature = \"x\"))]\n    B,\n}\n",
            &[],
            "Kind"
        ),
        "mod"
    );
}

/// 移動で同じ名前のメソッドを組にするときは、impl の対象の型ごとに比べる (移動先にある別の型の同じ
/// 名前のメソッドで、消えた設定を覆わない)。
#[test]
fn rust_item_cfg_moved_methods_are_compared_per_owner() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\n\npub struct S<T>(pub T);\n",
            ),
            (
                "src/a.rs",
                "pub fn keep() {}\n\nimpl crate::S<u8> {\n    pub fn f(&self) {}\n}\n",
            ),
            (
                "src/b.rs",
                "impl crate::S<u16> {\n    pub fn f(&self) -> u32 {\n        0\n    }\n}\n",
            ),
        ],
        &[
            ("src/a.rs", "pub fn keep() {}\n"),
            (
                "src/b.rs",
                "impl crate::S<u16> {\n    pub fn f(&self) -> u32 {\n        0\n    }\n}\n\n#[cfg(feature = \"x\")]\nimpl crate::S<u8> {\n    pub fn f(&self) {}\n}\n",
            ),
        ],
    );
    assert!(
        api.moved.iter().any(|symbol| symbol.name == "f"),
        "{:?}",
        api.moved
    );
    assert_eq!(
        classify(&api, "f"),
        ("rm_dead", cfg(&[&[]], &[&["cfg(feature = \"x\")"]]))
    );
}

/// 分岐を足した・消しただけとみなすのは、残った分岐の囲む impl の見出しも同じで、足した後の分岐が
/// どの設定でも並ばないときだけ。そうでなければ組を決められない変更として blocking に残す。
#[test]
fn rust_item_cfg_branch_additions_keep_remaining_contracts() {
    let modified = |old: &str, new: &str| {
        let api = api_changes(&[("src/lib.rs", old)], &[("src/lib.rs", new)]);
        api.modified
            .iter()
            .any(|change| change.name == "f" || change.name == "S.f")
    };
    // 残った分岐の impl の境界を強め、別の設定の分岐を足す
    assert!(modified(
        "pub struct S<T>(pub T);\n\n#[cfg(unix)]\nimpl<T> S<T> {\n    pub fn f(&self) {}\n}\n",
        "pub struct S<T>(pub T);\n\n#[cfg(unix)]\nimpl<T: Copy> S<T> {\n    pub fn f(&self) {}\n}\n\n#[cfg(windows)]\nimpl<T> S<T> {\n    pub fn f(&self, _: u8) {}\n}\n",
    ));
    // 既存の分岐と同じ設定に並ぶ分岐を足す
    assert!(modified(
        "pub fn f() {}\n",
        "pub fn f() {}\n\n#[cfg(unix)]\npub fn f(_: u8) {}\n",
    ));
    // 別の設定の分岐を足すだけなら、増える設定だけ
    let api = api_changes(
        &[("src/lib.rs", "#[cfg(unix)]\npub fn f() {}\n")],
        &[(
            "src/lib.rs",
            "#[cfg(unix)]\npub fn f() {}\n\n#[cfg(windows)]\npub fn f(_: u8) {}\n",
        )],
    );
    assert!(api.modified.is_empty(), "{:?}", api.modified);
    assert_eq!(classify(&api, "f").0, "add");
}

/// 移動先で impl の型の書き方を変えても (`crate::S<u8>` と `super::S<u8>`)、移動の組を決めた
/// signature の宣言どうしで有る設定を比べる。
#[test]
fn rust_item_cfg_moved_methods_follow_rewritten_owners() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\n\npub struct S<T>(pub T);\n",
            ),
            (
                "src/a.rs",
                "pub fn keep() {}\n\nimpl crate::S<u8> {\n    pub fn f(&self) {}\n}\n",
            ),
            (
                "src/b.rs",
                "impl crate::S<u16> {\n    pub fn f(&self) -> u32 {\n        0\n    }\n}\n",
            ),
        ],
        &[
            ("src/a.rs", "pub fn keep() {}\n"),
            (
                "src/b.rs",
                "impl crate::S<u16> {\n    pub fn f(&self) -> u32 {\n        0\n    }\n}\n\n#[cfg(feature = \"x\")]\nimpl super::S<u8> {\n    pub fn f(&self) {}\n}\n",
            ),
        ],
    );
    assert!(
        api.moved.iter().any(|symbol| symbol.name == "f"),
        "{:?}",
        api.moved
    );
    assert_eq!(
        classify(&api, "f"),
        ("rm_dead", cfg(&[&[]], &[&["cfg(feature = \"x\")"]]))
    );
}

/// 同じ signature の分岐でも、既存の分岐と同じ設定に並ぶ分岐を足すか、残った分岐を囲む impl の見出しを
/// 変えるなら、組を決められない変更として blocking に残す。
#[test]
fn rust_item_cfg_same_signature_branches_keep_contracts() {
    let modified = |old: &str, new: &str| {
        let api = api_changes(&[("src/lib.rs", old)], &[("src/lib.rs", new)]);
        api.modified
            .iter()
            .any(|change| change.name == "f" || change.name == "S.f")
    };
    // 同じ設定に並ぶ分岐を足す (unix で feature x を有効にすると 2 つ並ぶ)
    assert!(modified(
        "#[cfg(unix)]\npub fn f() {}\n",
        "#[cfg(unix)]\npub fn f() {}\n\n#[cfg(feature = \"x\")]\npub fn f() {}\n",
    ));
    // 残った分岐の impl の境界を強め、同じ signature の分岐を別の設定に足す
    assert!(modified(
        "pub struct S<T>(pub T);\n\n#[cfg(unix)]\nimpl<T> S<T> {\n    pub fn f(&self) {}\n}\n",
        "pub struct S<T>(pub T);\n\n#[cfg(unix)]\nimpl<T: Copy> S<T> {\n    pub fn f(&self) {}\n}\n\n#[cfg(windows)]\nimpl<T> S<T> {\n    pub fn f(&self) {}\n}\n",
    ));
}

/// 移動では、宣言を (impl の対象の型, signature) ごとに組にする。分岐の間で条件を入れ替えた移動と、
/// 複数の型の書き方をまとめて変えた移動でも、消える設定を見落とさない。
#[test]
fn rust_item_cfg_moves_pair_declarations_by_owner_and_signature() {
    let lib = "pub mod a;\npub mod b;\n\npub struct S<T>(pub T);\n";
    let removed = |old_a: &str, new_b: &str| {
        let api = api_changes(
            &[
                ("src/lib.rs", lib),
                ("src/a.rs", &format!("pub fn keep() {{}}\n\n{old_a}")),
                ("src/b.rs", "pub fn other() {}\n"),
            ],
            &[
                ("src/a.rs", "pub fn keep() {}\n"),
                ("src/b.rs", &format!("pub fn other() {{}}\n\n{new_b}")),
            ],
        );
        assert!(
            api.moved.iter().any(|symbol| symbol.name == "f"),
            "{:?}",
            api.moved
        );
        classify(&api, "f").0
    };
    // `#[cfg]` の分岐の条件を入れ替えて移す (unix の戻り値の型が変わる)
    assert_eq!(
        removed(
            "#[cfg(unix)]\npub fn f() -> u8 {\n    0\n}\n\n#[cfg(not(unix))]\npub fn f() -> u16 {\n    0\n}\n",
            "#[cfg(not(unix))]\npub fn f() -> u8 {\n    0\n}\n\n#[cfg(unix)]\npub fn f() -> u16 {\n    0\n}\n",
        ),
        "rm_dead"
    );
    // 2 つの型のメソッドを、型の書き方を変えて移し、片方にだけ条件を足す
    assert_eq!(
        removed(
            "impl crate::S<u8> {\n    pub fn f(&self) {}\n}\n\nimpl crate::S<u16> {\n    pub fn f(&self) -> u32 {\n        0\n    }\n}\n",
            "#[cfg(feature = \"x\")]\nimpl super::S<u8> {\n    pub fn f(&self) {}\n}\n\nimpl super::S<u16> {\n    pub fn f(&self) -> u32 {\n        0\n    }\n}\n",
        ),
        "rm_dead"
    );
}

/// 同じ signature の分岐の間で囲む impl の境界を入れ替えると、条件と見出しの対応が変わるので blocking
/// に残す。移動先で同じ設定に並ぶ宣言を足すのも (二重の定義) blocking。
#[test]
fn rust_item_cfg_header_swaps_and_moved_duplicates_stay_blocking() {
    let reason = |api: &ApiChanges| {
        api.modified
            .iter()
            .find(|change| change.name == "f" || change.name == "S.f")
            .and_then(|change| change.unverified.as_ref())
            .map(|unverified| unverified.reason)
    };
    let swap = |x: &str, other: &str| {
        format!(
            "pub struct S<T>(pub T);\n\n#[cfg(feature = \"x\")]\nimpl<T{x}> S<T> {{\n    pub fn f(&self) {{}}\n}}\n\n#[cfg(not(feature = \"x\"))]\nimpl<T{other}> S<T> {{\n    pub fn f(&self) {{}}\n}}\n"
        )
    };
    let api = api_changes(
        &[("src/lib.rs", &swap("", ": Copy"))],
        &[("src/lib.rs", &swap(": Copy", ""))],
    );
    assert_eq!(
        reason(&api),
        Some(ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous)
    );
    // 移動先で、移した関数と同じ設定に並ぶ同じ名前の関数を足す
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "pub fn keep() {}\n\npub fn f() {}\n"),
            ("src/b.rs", "pub fn other() {}\n"),
        ],
        &[
            ("src/a.rs", "pub fn keep() {}\n"),
            (
                "src/b.rs",
                "pub fn other() {}\n\npub fn f() {}\n\n#[cfg(feature = \"x\")]\npub fn f() {}\n",
            ),
        ],
    );
    assert_eq!(
        reason(&api),
        Some(ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous),
        "{:?} {:?}",
        api.moved,
        api.modified
    );
}
