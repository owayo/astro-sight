//! 自由関数・定数を別のファイルへ移すのと同時に、移さなければ互換と判定される変更をした差分を、
//! 移動と互換な変更として扱う (Issue #114)。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

const ALPHA: &str = "crates/alpha/src/items.rs";
const BETA: &str = "crates/beta/src/items.rs";
const WORKSPACE: &str = "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/consumer\"]\nresolver = \"2\"\n";

const WEEK_BEFORE: &str = "pub const WEEK: [(&str, u8); 2] = [(\"Mon\", 1), (\"Tue\", 2)];\n";
const WEEK_AFTER: &str = "const NAMES: [&str; 2] = [\"Mon\", \"Tue\"];\n\npub const WEEK: [(&str, u8); 2] = [(NAMES[0], 1), (NAMES[1], 2)];\n";
const WEEK_USE: &str = "println!(\"{}\", KRATE::items::WEEK.len());";

const COUNT_EXPANDED: &str = "use std::collections::HashMap;\n\npub fn count(marks: &HashMap<String, String>) -> usize {\n    marks.len()\n}\n";
const COUNT_ALIASED: &str = "use std::collections::HashMap;\n\npub type Tags = HashMap<String, String>;\n\npub fn count(marks: &Tags) -> usize {\n    marks.len()\n}\n";
const COUNT_USE: &str = "println!(\"{}\", KRATE::items::count(&std::collections::HashMap::new()));";

const DOUBLE: &str = "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n";
const CONST_DOUBLE: &str = "pub const fn double(x: i32) -> i32 {\n    x * 2\n}\n";
const DOUBLE_USE: &str = "println!(\"{}\", KRATE::items::double(2));";

/// 移した先のファイルを、変更前にどう置いておくか。
#[derive(Clone, Copy, Debug, PartialEq)]
enum Destination {
    /// 変更前から空のファイルがある (Issue の再現)。git は rename を検出できず、移動元の削除と
    /// 移動先の変更として渡す。
    Existing,
    /// 変更前には無い (未追跡の新しいファイル)。中身が似ていれば git は rename として渡し、
    /// 移さない変更と同じ経路で判定される (moved には出ない)。
    Untracked,
}

const DESTINATIONS: [Destination; 2] = [Destination::Existing, Destination::Untracked];

/// 1 つの宣言に対する変更。`*_use` は consumer の `main` の本体で、`KRATE` を使うクレートに
/// 置き換える。
struct Change<'a> {
    name: &'a str,
    before: &'a str,
    after: &'a str,
    old_use: &'a str,
    new_use: &'a str,
}

impl<'a> Change<'a> {
    fn new(name: &'a str, before: &'a str, after: &'a str, uses: &'a str) -> Self {
        Self {
            name,
            before,
            after,
            old_use: uses,
            new_use: uses,
        }
    }
}

fn package(name: &str, dependencies: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}"
    )
}

fn consumer(krate: &str, uses: &str) -> String {
    format!("fn main() {{\n    {}\n}}\n", uses.replace("KRATE", krate))
}

/// alpha・beta・consumer の 3 クレートの変更前をコミットした一時リポジトリ。
fn base_repo(change: &Change<'_>, destination: Destination) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut files = vec![
        ("Cargo.toml", WORKSPACE.to_owned()),
        ("crates/alpha/Cargo.toml", package("alpha", "")),
        ("crates/beta/Cargo.toml", package("beta", "")),
        (
            "crates/consumer/Cargo.toml",
            package("consumer", "alpha = { path = \"../alpha\" }\n"),
        ),
        ("crates/alpha/src/lib.rs", "pub mod items;\n".to_owned()),
        (ALPHA, change.before.to_owned()),
        (
            "crates/consumer/src/main.rs",
            consumer("alpha", change.old_use),
        ),
    ];
    match destination {
        Destination::Existing => {
            files.push(("crates/beta/src/lib.rs", "pub mod items;\n".to_owned()));
            files.push((BETA, String::new()));
        }
        Destination::Untracked => files.push(("crates/beta/src/lib.rs", String::new())),
    }
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (*path, content.as_str()))
        .collect();
    git_commit_files(repo, &files, "base");
    dir
}

/// alpha の items.rs を消し、beta の items.rs へ `items` を書いて consumer を beta に向ける。
fn move_to_beta(repo: &std::path::Path, items: &str, new_use: &str) {
    git_in(repo, &["rm", "-q", ALPHA]);
    fs::write(repo.join("crates/alpha/src/lib.rs"), "").unwrap();
    fs::write(repo.join("crates/beta/src/lib.rs"), "pub mod items;\n").unwrap();
    fs::write(repo.join(BETA), items).unwrap();
    fs::write(
        repo.join("crates/consumer/Cargo.toml"),
        package("consumer", "beta = { path = \"../beta\" }\n"),
    )
    .unwrap();
    fs::write(
        repo.join("crates/consumer/src/main.rs"),
        consumer("beta", new_use),
    )
    .unwrap();
}

/// 宣言を alpha から beta へ移して変更した作業ツリーの API 差分。
fn api_after_move(change: &Change<'_>, destination: Destination) -> ApiChanges {
    let dir = base_repo(change, destination);
    move_to_beta(dir.path(), change.after, change.new_use);
    detect_api_changes_from_worktree(dir.path())
}

/// 移さずに、alpha の items.rs の中で同じ変更をした作業ツリーの API 差分。
fn api_in_place(change: &Change<'_>) -> ApiChanges {
    let dir = base_repo(change, Destination::Existing);
    fs::write(dir.path().join(ALPHA), change.after).unwrap();
    fs::write(
        dir.path().join("crates/consumer/src/main.rs"),
        consumer("alpha", change.new_use),
    )
    .unwrap();
    detect_api_changes_from_worktree(dir.path())
}

fn is_moved(api: &ApiChanges, name: &str) -> bool {
    api.moved
        .iter()
        .any(|m| m.name == name && m.from == ALPHA && m.to == BETA)
}

fn is_removed(api: &ApiChanges, name: &str) -> bool {
    api.removed
        .iter()
        .any(|s| s.name == name && s.file == ALPHA)
}

/// 移さないときの判定のうち、互換と確定した区分。
#[derive(Debug, PartialEq)]
enum Compatibility {
    /// 値だけの変更 (const_value)。
    ConstValue,
    /// 互換な変更 (mod_compat)。理由を添える。
    Compatible(String),
}

/// `file` にある `name` の、互換と確定した区分。どちらでもなければ None。
fn compatibility(api: &ApiChanges, name: &str, file: &str) -> Option<Compatibility> {
    if api
        .const_value_changes
        .iter()
        .any(|c| c.name == name && c.file == file)
    {
        return Some(Compatibility::ConstValue);
    }
    api.compatible_modified
        .iter()
        .find(|c| c.name == name && c.file == file)
        .map(|c| Compatibility::Compatible(c.reason.clone()))
}

/// 移動が互換な区分 (`expected`) になり、移動元の削除・移動先の追加・api.mod に出ない。
/// 削除と変更として渡る差分 (`Existing`) では moved にもなる。
fn assert_moved_compatibly(
    api: &ApiChanges,
    name: &str,
    expected: &Compatibility,
    destination: Destination,
    label: &str,
) {
    if destination == Destination::Existing {
        assert!(is_moved(api, name), "{label}: {api:?}");
    }
    assert_eq!(
        compatibility(api, name, BETA).as_ref(),
        Some(expected),
        "{label}: {api:?}"
    );
    for (bucket, symbols) in [
        ("removed", &api.removed),
        ("removed_dead", &api.removed_dead),
        ("added", &api.added),
    ] {
        assert!(
            symbols.iter().all(|s| s.name != name),
            "{label}: {name} が {bucket} に出た: {api:?}"
        );
    }
    for changes in [&api.modified, &api.modified_closed_in_diff] {
        assert!(changes.iter().all(|c| c.name != name), "{label}: {api:?}");
    }
}

/// 移さなければ互換と判定されない変更は、組にせず今と同じく移動元を api.rm に残す。
fn assert_kept_removed(api: &ApiChanges, name: &str, label: &str) {
    assert!(is_removed(api, name), "{label}: {api:?}");
    assert!(api.moved.iter().all(|m| m.name != name), "{label}: {api:?}");
    assert_eq!(compatibility(api, name, BETA), None, "{label}: {api:?}");
    assert!(
        api.modified_closed_in_diff.iter().all(|c| c.name != name),
        "{label}: {api:?}"
    );
}

/// 別の新定義へ全参照が向く場合の rm_dead は許すが、移動・互換の証明にはしない。
fn assert_rejected_move(api: &ApiChanges, name: &str, label: &str) {
    let deleted = api
        .removed
        .iter()
        .chain(&api.removed_dead)
        .filter(|s| s.name == name && s.file == ALPHA)
        .count();
    assert_eq!(deleted, 1, "{label}: {api:?}");
    assert!(api.moved.iter().all(|m| m.name != name), "{label}: {api:?}");
    assert_eq!(compatibility(api, name, BETA), None, "{label}: {api:?}");
    assert!(
        api.modified_closed_in_diff.iter().all(|c| c.name != name),
        "{label}: {api:?}"
    );
}

/// 移さなければ互換と判定する A/B の fixture を、移した場合の同じ分類と照合する。
/// 非互換の fixture に使うのは旧 API を指す参照が残る場合だけで、`Existing` では api.rm を求める。
/// 全参照が別定義を指す削除の区分は `assert_rejected_move` で調べる。
fn assert_same_rule_as_in_place(change: &Change<'_>) -> Option<Compatibility> {
    let expected = compatibility(&api_in_place(change), change.name, ALPHA);
    for destination in DESTINATIONS {
        let api = api_after_move(change, destination);
        let label = format!("{destination:?} {}", change.after);
        match &expected {
            Some(expected) => {
                assert_moved_compatibly(&api, change.name, expected, destination, &label);
            }
            None if destination == Destination::Existing => {
                assert_kept_removed(&api, change.name, &label);
            }
            None => {}
        }
    }
    expected
}

/// (A) 定数を移し、初期化の式だけを書き換える (型と値は同じ)。移さないときと同じく
/// const_value になり、移動元の削除は moved に変わる。
#[test]
fn moved_const_with_initializer_change_is_moved_and_const_value() {
    let change = Change::new("WEEK", WEEK_BEFORE, WEEK_AFTER, WEEK_USE);
    assert_eq!(
        assert_same_rule_as_in_place(&change),
        Some(Compatibility::ConstValue)
    );
}

/// (B) 関数を移し、引数の型を移動先に足した `pub type` の別名で書き直す。逆に、移動元にだけ
/// 別名があり移動先で展開した形に戻す変種も、移さないときと同じく型別名の互換になる。
#[test]
fn moved_function_with_alias_rewrite_is_moved_and_compatible() {
    for (before, after) in [
        (COUNT_EXPANDED, COUNT_ALIASED),
        (COUNT_ALIASED, COUNT_EXPANDED),
    ] {
        let change = Change::new("count", before, after, COUNT_USE);
        assert_eq!(
            assert_same_rule_as_in_place(&change),
            Some(Compatibility::Compatible(
                "transparent_rust_type_alias".to_owned()
            )),
            "{after}"
        );
    }
}

/// (C) 何も変えずに移す・(D) 本体だけを変えて移すのは、今と同じく moved だけ
/// (rename として渡れば何も出ない)。
#[test]
fn moved_function_without_signature_change_stays_moved() {
    for after in [DOUBLE, "pub fn double(x: i32) -> i32 {\n    x + x\n}\n"] {
        for destination in DESTINATIONS {
            let api = api_after_move(
                &Change::new("double", DOUBLE, after, DOUBLE_USE),
                destination,
            );
            let label = format!("{destination:?} {after}");
            assert_eq!(
                is_moved(&api, "double"),
                destination == Destination::Existing,
                "{label}: {api:?}"
            );
            assert_eq!(
                compatibility(&api, "double", BETA),
                None,
                "{label}: {api:?}"
            );
            for symbols in [&api.removed, &api.removed_dead, &api.added] {
                assert!(
                    symbols.iter().all(|s| s.name != "double"),
                    "{label}: {api:?}"
                );
            }
        }
    }
}

/// 移さなければ互換と判定されない変更は組にしない: (F) の公開範囲の縮小、呼び出しを直した
/// 必須引数の追加 (移さなければ mod_closed)。
#[test]
fn moved_incompatible_change_follows_the_in_place_rule() {
    let round_half = |vis: &str| {
        format!(
            "{vis} fn round_half(x: f64) -> f64 {{\n    (x * 2.0).round() / 2.0\n}}\n\npub fn price(x: f64) -> f64 {{\n    round_half(x)\n}}\n"
        )
    };
    let (round_half_before, round_half_after) = (round_half("pub"), round_half("pub(crate)"));
    let price_use = "println!(\"{}\", KRATE::items::price(1.2));";
    let narrowed = Change::new(
        "round_half",
        &round_half_before,
        &round_half_after,
        price_use,
    );

    let widened = Change {
        name: "double",
        before: DOUBLE,
        after: "pub fn double(x: i32, times: i32) -> i32 {\n    x * times\n}\n",
        old_use: DOUBLE_USE,
        new_use: "println!(\"{}\", KRATE::items::double(2, 2));",
    };
    let in_place = api_in_place(&widened);
    assert!(
        in_place
            .modified_closed_in_diff
            .iter()
            .any(|c| c.name == "double"),
        "{in_place:?}"
    );
    for change in [&narrowed, &widened] {
        assert_eq!(
            compatibility(&api_in_place(change), change.name, ALPHA),
            None
        );
        for destination in DESTINATIONS {
            let api = api_after_move(change, destination);
            if destination == Destination::Existing {
                assert_rejected_move(&api, change.name, change.after);
            } else {
                assert!(api.moved.iter().all(|m| m.name != change.name), "{api:?}");
                assert_eq!(compatibility(&api, change.name, BETA), None, "{api:?}");
            }
        }
    }
}

#[test]
fn moved_const_addition_preserves_local_type_ownership() {
    let side_label = |qualifier: &str| {
        format!(
            "pub enum Side {{\n    Buy,\n    Sell,\n}}\n\npub {qualifier}fn side_label(side: Side) -> &'static str {{\n    match side {{\n        Side::Buy => \"buy\",\n        Side::Sell => \"sell\",\n    }}\n}}\n"
        )
    };
    let (side_before, side_after) = (side_label(""), side_label("const "));
    let change = Change::new(
        "side_label",
        &side_before,
        &side_after,
        "println!(\"{}\", KRATE::items::side_label(KRATE::items::Side::Buy));",
    );
    assert_eq!(
        compatibility(&api_in_place(&change), "side_label", ALPHA),
        Some(Compatibility::Compatible("rust_const_fn_added".to_owned()))
    );
    let api = api_after_move(&change, Destination::Existing);
    assert_kept_removed(&api, "side_label", "local enum identity");
}

#[test]
fn moved_const_addition_proves_primitive_function_contract() {
    let change = Change::new("double", DOUBLE, CONST_DOUBLE, DOUBLE_USE);
    let expected = Compatibility::Compatible("rust_const_fn_added".to_owned());
    assert_eq!(
        compatibility(&api_in_place(&change), "double", ALPHA),
        Some(expected)
    );
    let api = api_after_move(&change, Destination::Existing);
    assert_moved_compatibly(
        &api,
        "double",
        &Compatibility::Compatible("rust_const_fn_added".to_owned()),
        Destination::Existing,
        "const addition",
    );
}

#[test]
fn moved_const_addition_rejects_other_header_changes_and_opaque_returns() {
    for (before, after, uses) in [
        (
            DOUBLE,
            "pub const fn double(x: i32) -> i64 { x as i64 }\n",
            DOUBLE_USE,
        ),
        (
            DOUBLE,
            "pub const fn double(x: i32, times: i32) -> i32 { x * times }\n",
            "println!(\"{}\", KRATE::items::double(2, 2));",
        ),
        (CONST_DOUBLE, DOUBLE, DOUBLE_USE),
        (
            "pub fn double(x: i32) -> impl Copy { x }\n",
            "pub const fn double(x: i32) -> impl Copy { x }\n",
            "let _ = KRATE::items::double(2);",
        ),
    ] {
        let change = Change::new("double", before, after, uses);
        let api = api_after_move(&change, Destination::Existing);
        assert_rejected_move(&api, "double", after);
    }
}

#[test]
fn moved_const_addition_within_one_package_is_proven() {
    let change = Change::new("double", DOUBLE, CONST_DOUBLE, DOUBLE_USE);
    let dir = base_repo(&change, Destination::Existing);
    let repo = dir.path();
    let destination = "crates/alpha/src/new_items.rs";
    git_commit_files(
        repo,
        &[
            (
                "crates/alpha/src/lib.rs",
                "pub mod items;\npub mod new_items;\n",
            ),
            (destination, ""),
        ],
        "second module",
    );
    git_in(repo, &["rm", "-q", ALPHA]);
    fs::write(repo.join("crates/alpha/src/lib.rs"), "pub mod new_items;\n").unwrap();
    fs::write(repo.join(destination), CONST_DOUBLE).unwrap();
    fs::write(
        repo.join("crates/consumer/src/main.rs"),
        "fn main() { let _ = alpha::new_items::double(2); }\n",
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.moved
            .iter()
            .any(|m| m.name == "double" && m.from == ALPHA && m.to == destination),
        "{api:?}"
    );
    assert_eq!(
        compatibility(&api, "double", destination),
        Some(Compatibility::Compatible("rust_const_fn_added".to_owned()))
    );
    assert!(api.removed.iter().all(|s| s.name != "double"), "{api:?}");
}

#[test]
fn moved_const_addition_keeps_same_file_callers_and_rejects_unproven_changes() {
    for (function, caller, accepted) in [
        (
            "pub const fn f(x: u8) -> u8 { x }\n",
            "pub fn g() -> u8 {\n    f(1)\n}\n",
            true,
        ),
        (
            "pub const fn f(x: u16) -> u8 { x as u8 }\n",
            "pub fn g() -> u8 {\n    f(1)\n}\n",
            false,
        ),
        (
            "pub const fn f(x: u8) -> u8 { x }\n",
            "pub fn g() -> u8 {\n    crate::old::f(1)\n}\n",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", &package("same_file_callers", "")),
                ("src/lib.rs", "pub mod old;\npub mod new;\n"),
                (
                    "src/old.rs",
                    "pub fn f(x: u8) -> u8 { x }\npub fn g() -> u8 {\n    f(1)\n}\n",
                ),
                ("src/new.rs", ""),
            ],
            "base",
        );
        fs::remove_file(repo.join("src/old.rs")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub mod new;\n").unwrap();
        fs::write(repo.join("src/new.rs"), format!("{function}{caller}")).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        let paired = |name| {
            api.moved
                .iter()
                .any(|m| m.name == name && m.from == "src/old.rs" && m.to == "src/new.rs")
        };
        assert_eq!(paired("f"), accepted, "{function}{caller}: {api:?}");
        assert!(paired("g"), "{api:?}");
        assert_eq!(
            compatibility(&api, "f", "src/new.rs"),
            accepted.then(|| Compatibility::Compatible("rust_const_fn_added".to_owned())),
            "{api:?}"
        );
        let stale = caller.contains("crate::old::f");
        assert_eq!(api.removed.iter().any(|s| s.name == "f"), stale, "{api:?}");
        assert_eq!(
            api.removed_dead.iter().any(|s| s.name == "f"),
            !accepted && !stale,
            "{api:?}"
        );
    }
}

#[test]
fn moved_compatible_change_uses_directory_relative_old_inventory() {
    for change in [
        Change::new("WEEK", WEEK_BEFORE, WEEK_AFTER, "WEEK.len()"),
        Change::new(
            "count",
            COUNT_EXPANDED,
            COUNT_ALIASED,
            "count(&std::collections::HashMap::new())",
        ),
        Change::new("double", DOUBLE, CONST_DOUBLE, "double(2)"),
    ] {
        for updated in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            init_git_repo_for_test(repo);
            let read = |module: &str| {
                format!(
                    "pub mod {module};\npub mod new;\npub fn read() {{\n    let _ = crate::{module}::{};\n}}\n",
                    change.old_use,
                )
            };
            git_commit_files(
                repo,
                &[
                    ("pkg/Cargo.toml", &package("subdirectory", "")),
                    ("pkg/src/lib.rs", &read("old")),
                    ("pkg/src/old.rs", change.before),
                    ("pkg/src/new.rs", ""),
                    // 解析 root 外の動的ソース構成は、この inventory の監査対象に入れない。
                    ("outside.rs", "include!(\"unavailable.rs\");\n"),
                ],
                "base",
            );
            fs::remove_file(repo.join("pkg/src/old.rs")).unwrap();
            fs::write(repo.join("pkg/src/new.rs"), change.after).unwrap();
            let lib = format!(
                "pub mod new;\npub fn read() {{\n    let _ = crate::{}::{};\n}}\n",
                if updated { "new" } else { "old" },
                change.new_use,
            );
            fs::write(repo.join("pkg/src/lib.rs"), lib).unwrap();
            let api = detect_api_changes_from_worktree(&repo.join("pkg"));
            assert_eq!(
                api.moved.iter().any(|m| {
                    m.name == change.name && m.from == "src/old.rs" && m.to == "src/new.rs"
                }),
                updated,
                "{}, updated={updated}: {api:?}",
                change.name,
            );
            assert_eq!(
                api.removed.iter().any(|s| s.name == change.name),
                !updated,
                "{api:?}"
            );
            assert_eq!(
                compatibility(&api, change.name, "src/new.rs").is_some(),
                updated,
                "{api:?}"
            );
        }
    }
}

/// 移動元と移動先の対応が 1 対 1 に決まらなければ、今と同じく api.rm に残す。
#[test]
fn moved_compatible_change_with_ambiguous_correspondence_stays_removed() {
    let change = Change::new("WEEK", WEEK_BEFORE, WEEK_AFTER, WEEK_USE);
    let other_week = "pub const WEEK: [(&str, u8); 2] = [(\"Sat\", 6), (\"Sun\", 7)];\n";
    let cfg_weeks = "#[cfg(unix)]\npub const WEEK: [(&str, u8); 2] = [(\"mon\", 1), (\"tue\", 2)];\n#[cfg(not(unix))]\npub const WEEK: [(&str, u8); 2] = [(\"MON\", 1), (\"TUE\", 2)];\n";
    for label in [
        "移動先の候補が 2 ファイルにある",
        "移動先のファイルに同じ名前の宣言が 2 件ある",
        "同じ名前・種別の削除がほかのファイルにもある",
    ] {
        let dir = base_repo(&change, Destination::Existing);
        let repo = dir.path();
        if label.starts_with("同じ名前・種別の削除") {
            // 変更前に alpha の別のモジュールにも同じ名前の定数があり、それも消す。
            fs::write(repo.join("crates/alpha/src/more.rs"), other_week).unwrap();
            fs::write(
                repo.join("crates/alpha/src/lib.rs"),
                "pub mod items;\npub mod more;\n",
            )
            .unwrap();
            git_in(repo, &["add", "-A"]);
            git_in(repo, &["commit", "-q", "-m", "more"]);
        }
        let items = if label.starts_with("移動先のファイル") {
            cfg_weeks
        } else {
            WEEK_AFTER
        };
        move_to_beta(repo, items, WEEK_USE);
        if label.starts_with("移動先の候補") {
            fs::write(
                repo.join("crates/beta/src/lib.rs"),
                "pub mod items;\npub mod other;\n",
            )
            .unwrap();
            fs::write(repo.join("crates/beta/src/other.rs"), other_week).unwrap();
        }
        if label.starts_with("同じ名前・種別の削除") {
            fs::write(repo.join("crates/alpha/src/more.rs"), "").unwrap();
        }
        let api = detect_api_changes_from_worktree(repo);
        assert_kept_removed(&api, "WEEK", label);
        assert!(
            api.const_value_changes.iter().all(|c| c.name != "WEEK"),
            "{label}: {api:?}"
        );
    }
}

/// ライブラリのクレートからバイナリだけのクレートへ移すと、ライブラリの公開 API は消える。
/// 移さないときの判定 (bin-only は API 差分にしない) を組の判定に使っても、削除を隠さない。
#[test]
fn moved_compatible_change_into_binary_crate_stays_removed() {
    let change = Change::new("WEEK", WEEK_BEFORE, WEEK_AFTER, WEEK_USE);
    let dir = base_repo(&change, Destination::Existing);
    let repo = dir.path();
    git_in(repo, &["rm", "-q", ALPHA]);
    fs::write(repo.join("crates/alpha/src/lib.rs"), "").unwrap();
    fs::write(
        repo.join("crates/consumer/Cargo.toml"),
        package("consumer", ""),
    )
    .unwrap();
    fs::write(
        repo.join("crates/consumer/src/main.rs"),
        format!("{WEEK_AFTER}\nfn main() {{\n    println!(\"{{}}\", WEEK.len());\n}}\n"),
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(is_removed(&api, "WEEK"), "{api:?}");
    assert!(api.moved.iter().all(|m| m.name != "WEEK"), "{api:?}");
    assert!(
        api.const_value_changes.iter().all(|c| c.name != "WEEK"),
        "{api:?}"
    );
}

/// 互換な宣言変更でも、消した旧パスを呼ぶ参照が残れば削除を隠さない。
#[test]
fn moved_compatible_change_with_stale_caller_stays_removed() {
    for change in [
        Change::new("WEEK", WEEK_BEFORE, WEEK_AFTER, WEEK_USE),
        Change::new("count", COUNT_EXPANDED, COUNT_ALIASED, COUNT_USE),
        Change::new("double", DOUBLE, CONST_DOUBLE, DOUBLE_USE),
    ] {
        for suffix in ["", " // caller changed, old path retained"] {
            let dir = base_repo(&change, Destination::Existing);
            move_to_beta(dir.path(), change.after, change.new_use);
            fs::write(
                dir.path().join("crates/consumer/src/main.rs"),
                consumer("alpha", &format!("{}{suffix}", change.old_use)),
            )
            .unwrap();
            let api = detect_api_changes_from_worktree(dir.path());
            assert_kept_removed(&api, change.name, change.old_use);
        }
    }
}

#[test]
fn moved_compatible_change_with_raw_dependency_alias_stays_removed() {
    assert_dependency_alias_stays_removed("beta", "r#beta", "beta = { path = \"../beta\" }\n");
}

#[test]
fn moved_proof_with_shadowed_primitive_stays_removed() {
    for change in [
        Change::new(
            "WEEK",
            "pub const WEEK: u8 = 1;\n",
            "pub type u8 = u16;\npub const WEEK: u8 = 1 + 0;\n",
            "let _: u8 = KRATE::items::WEEK;",
        ),
        Change::new(
            "take",
            "pub fn take(_: u8) {}\n",
            "pub type u8 = u16;\npub type Value = u8;\npub fn take(_: Value) {}\n",
            "KRATE::items::take(1u8);",
        ),
    ] {
        let api = api_after_move(&change, Destination::Existing);
        if change.name == "take" {
            assert_rejected_move(&api, change.name, "primitive shadowing");
        } else {
            assert_kept_removed(&api, change.name, "primitive shadowing");
        }
    }
}

#[test]
fn moved_proof_with_raw_nominal_type_stays_removed() {
    let change = Change::new(
        "WEEK",
        "pub struct r#Day;\npub const WEEK: Option<Day> = None;\n",
        "pub struct r#Day;\npub const WEEK: Option<Day> = None::<Day>;\n",
        "let _: Option<alpha::items::Day> = KRATE::items::WEEK;",
    );
    let dir = base_repo(&change, Destination::Existing);
    let repo = dir.path();
    move_to_beta(repo, change.after, change.new_use);
    fs::write(repo.join(ALPHA), "pub struct r#Day;\n").unwrap();
    fs::write(repo.join("crates/alpha/src/lib.rs"), "pub mod items;\n").unwrap();
    fs::write(
        repo.join("crates/consumer/Cargo.toml"),
        package(
            "consumer",
            "alpha = { path = \"../alpha\" }\nbeta = { path = \"../beta\" }\n",
        ),
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert_kept_removed(&api, "WEEK", "raw nominal type");
}

#[test]
fn moved_proof_requires_nonoptional_destination_dependency() {
    let change = Change::new(
        "WEEK",
        "pub const WEEK: u8 = 1;\n",
        "pub const WEEK: u8 = 1 + 0;\n",
        "let _: u8 = KRATE::items::WEEK;",
    );
    for (optional, features, accepted) in [
        ("true", "", false),
        ("true", "\n[features]\ndefault = [\"dep:beta\"]\n", false),
        ("false", "", true),
    ] {
        let dir = base_repo(&change, Destination::Existing);
        move_to_beta(dir.path(), change.after, change.new_use);
        fs::write(
            dir.path().join("crates/consumer/Cargo.toml"),
            format!(
                "{}{features}",
                package(
                    "consumer",
                    &format!("beta = {{ path = \"../beta\", optional = {optional} }}\n")
                )
            ),
        )
        .unwrap();
        let api = detect_api_changes_from_worktree(dir.path());
        if accepted {
            assert_moved_compatibly(
                &api,
                "WEEK",
                &Compatibility::ConstValue,
                Destination::Existing,
                "optional=false",
            );
        } else {
            assert_kept_removed(&api, "WEEK", "optional dependency");
        }
    }
}

#[test]
fn moved_compatible_change_with_nfc_dependency_alias_stays_removed() {
    assert_dependency_alias_stays_removed(
        "K",
        "K",
        "K = { package = \"beta\", path = \"../beta\" }\n",
    );
}

fn assert_dependency_alias_stays_removed(root: &str, binding: &str, beta_dependency: &str) {
    let change = Change::new(
        "WEEK",
        "pub const WEEK: u8 = 1;\n",
        "pub const WEEK: u8 = 1 + 0;\n",
        "let _: u8 = KRATE::items::WEEK;",
    );
    let dir = base_repo(&change, Destination::Existing);
    let repo = dir.path();
    let source = format!(
        "use alpha as {binding};\nfn main() {{\n    let _: u8 = {root}::items::WEEK;\n}}\n"
    );
    git_commit_files(repo, &[("crates/consumer/src/main.rs", &source)], "alias");
    move_to_beta(repo, change.after, change.new_use);
    fs::write(repo.join("crates/consumer/src/main.rs"), source).unwrap();
    fs::write(
        repo.join("crates/consumer/Cargo.toml"),
        package(
            "consumer",
            &format!("alpha = {{ path = \"../alpha\" }}\n{beta_dependency}"),
        ),
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert_kept_removed(&api, "WEEK", binding);
}

#[test]
fn moved_compatible_change_with_shadowed_standard_import_stays_removed() {
    let prefix = "pub mod std { pub mod types { pub struct Id; } }\nuse std::types::Id;\n";
    for (name, before, after, uses) in [
        (
            "take",
            "pub fn take(_: Id) {}\n",
            "pub type Value = Id;\npub fn take(_: Value) {}\n",
            "KRATE::items::take(alpha::items::std::types::Id);",
        ),
        (
            "WEEK",
            "pub const WEEK: Option<Id> = None;\n",
            "pub const WEEK: Option<Id> = Some(Id);\n",
            "let _: Option<alpha::items::std::types::Id> = KRATE::items::WEEK;",
        ),
    ] {
        let before = format!("{prefix}{before}");
        let after = format!("{prefix}{after}");
        let change = Change::new(name, &before, &after, uses);
        let dir = base_repo(&change, Destination::Existing);
        let repo = dir.path();
        move_to_beta(repo, &after, uses);
        fs::write(repo.join(ALPHA), prefix).unwrap();
        fs::write(repo.join("crates/alpha/src/lib.rs"), "pub mod items;\n").unwrap();
        fs::write(
            repo.join("crates/consumer/Cargo.toml"),
            package(
                "consumer",
                "alpha = { path = \"../alpha\" }\nbeta = { path = \"../beta\" }\n",
            ),
        )
        .unwrap();
        let api = detect_api_changes_from_worktree(repo);
        if name == "take" {
            assert_rejected_move(&api, name, "local mod std");
        } else {
            assert_kept_removed(&api, name, "local mod std");
        }
    }
}

#[test]
fn moved_rejected_function_with_old_path_caller_stays_blocking() {
    for change in [
        Change {
            name: "take",
            before: "pub fn take(_: u8) {}\n",
            after: "pub type u8 = u16;\npub type Value = u8;\npub fn take(_: Value) {}\n",
            old_use: "KRATE::items::take(1u8);",
            new_use: "alpha::items::take(1u8);",
        },
        Change {
            name: "double",
            before: "pub fn double(x: i32) -> impl Copy { x }\n",
            after: "pub const fn double(x: i32) -> impl Copy { x }\n",
            old_use: "let _ = KRATE::items::double(2);",
            new_use: "let _ = alpha::items::double(2);",
        },
        Change {
            name: "take",
            before: "pub mod std { pub mod types { pub struct Id; } }\nuse std::types::Id;\npub fn take(_: Id) {}\n",
            after: "pub mod std { pub mod types { pub struct Id; } }\nuse std::types::Id;\npub type Value = Id;\npub fn take(_: Value) {}\n",
            old_use: "KRATE::items::take(KRATE::items::std::types::Id);",
            new_use: "alpha::items::take(alpha::items::std::types::Id);",
        },
    ] {
        let api = api_after_move(&change, Destination::Existing);
        assert_kept_removed(&api, change.name, "old path caller");
        assert!(
            api.removed_dead.iter().all(|s| s.name != change.name),
            "{api:?}"
        );
    }
}

#[test]
fn moved_compatible_change_with_legacy_sysroot_dependency_stays_removed() {
    let before = "use std::fmt::Error;\npub fn take(_: Error) {}\n";
    let after = "use std::fmt::Error;\npub type Value = Error;\npub fn take(_: Value) {}\n";
    let uses = "KRATE::items::take(alpha::make_error());";
    let change = Change::new("take", before, after, uses);
    for table in ["dev_dependencies", "build_dependencies"] {
        let dir = base_repo(&change, Destination::Existing);
        let repo = dir.path();
        let beta_manifest = format!(
            "{}\n[{table}]\nstd = {{ package = \"fake_std\", path = \"../../../fake_std\" }}\n",
            package("beta", "alpha = { path = \"../alpha\" }\n").replace("2024", "2021")
        );
        let alpha_root =
            "pub mod items;\npub fn make_error() -> std::fmt::Error { std::fmt::Error }\n";
        git_commit_files(
            repo,
            &[
                ("crates/alpha/src/lib.rs", alpha_root),
                ("crates/beta/Cargo.toml", &beta_manifest),
                (
                    "crates/beta/src/lib.rs",
                    "pub mod items;\npub fn call() { alpha::items::take(alpha::make_error()); }\n",
                ),
            ],
            "legacy std dependency",
        );
        move_to_beta(repo, after, uses);
        fs::write(
            repo.join("crates/alpha/src/lib.rs"),
            "pub fn make_error() -> std::fmt::Error { std::fmt::Error }\n",
        )
        .unwrap();
        fs::write(
            repo.join("crates/beta/src/lib.rs"),
            "pub mod items;\npub fn call() { crate::items::take(alpha::make_error()); }\n",
        )
        .unwrap();
        fs::write(
            repo.join("crates/consumer/Cargo.toml"),
            package(
                "consumer",
                "alpha = { path = \"../alpha\" }\nbeta = { path = \"../beta\" }\n",
            ),
        )
        .unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert_kept_removed(&api, "take", table);
    }
}

#[test]
fn moved_compatible_change_accepts_a_direct_renamed_path_dependency() {
    let change = Change::new("WEEK", WEEK_BEFORE, WEEK_AFTER, WEEK_USE);
    let dir = base_repo(&change, Destination::Existing);
    move_to_beta(dir.path(), change.after, change.new_use);
    fs::write(
        dir.path().join("crates/consumer/Cargo.toml"),
        package(
            "consumer",
            "moved_core = { package = \"beta\", path = \"../beta\" }\n",
        ),
    )
    .unwrap();
    fs::write(
        dir.path().join("crates/consumer/src/main.rs"),
        consumer("moved_core", WEEK_USE),
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(dir.path());
    assert_moved_compatibly(
        &api,
        "WEEK",
        &Compatibility::ConstValue,
        Destination::Existing,
        "renamed dependency",
    );
}

/// 字面が移動先でも、束縛・生成コード・未解析ソース・別配置を確かめられなければ削除を残す。
#[test]
fn moved_compatible_change_requires_a_proven_destination_reference() {
    for label in [
        "use alias",
        "block binding",
        "module binding",
        "unknown macro",
        "unknown attribute",
        "path attribute",
        "two crate roots",
        "wrong dependency",
        "cargo config",
        "unparsed source",
        "unsupported source",
        "nested macro",
        "nested attribute",
        "macro import",
        "private destination",
        "custom target",
        "same package binary caller",
        "binary destination in library package",
        "block module",
        "macro block module",
    ] {
        let change = Change::new("WEEK", WEEK_BEFORE, WEEK_AFTER, WEEK_USE);
        let dir = base_repo(&change, Destination::Existing);
        let repo = dir.path();
        move_to_beta(repo, change.after, change.new_use);
        let caller = consumer("beta", WEEK_USE);
        match label {
            "use alias" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                format!("use alpha as beta;\n{caller}"),
            ),
            "block binding" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                caller.replace("fn main() {", "fn main() { let beta = 0;"),
            ),
            "module binding" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                format!("mod beta {{}}\n{caller}"),
            ),
            "unknown macro" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                format!("inject!();\n{caller}"),
            ),
            "unknown attribute" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                format!("#[inject]\nfn helper() {{}}\n{caller}"),
            ),
            "path attribute" => fs::write(
                repo.join("crates/beta/src/lib.rs"),
                "pub mod items;\n#[path = \"items.rs\"] pub mod duplicate;\n",
            ),
            "two crate roots" => fs::write(
                repo.join("crates/beta/src/main.rs"),
                "mod items;\nfn main() {}\n",
            ),
            "wrong dependency" => fs::write(
                repo.join("crates/consumer/Cargo.toml"),
                package(
                    "consumer",
                    "beta = { package = \"alpha\", path = \"../alpha\" }\n",
                ),
            ),
            "cargo config" => {
                fs::create_dir_all(repo.join(".cargo")).unwrap();
                fs::write(repo.join(".cargo/config.toml"), "[build]\nrustflags = []\n")
            }
            "unparsed source" => fs::write(
                repo.join("crates/beta/src/broken.rs"),
                "pub fn broken(WEEK: (",
            ),
            "unsupported source" => {
                fs::write(
                    repo.join(BETA),
                    format!("pub const AAA: u8 = 0;\n{WEEK_AFTER}"),
                )
                .unwrap();
                fs::write(repo.join("view.vue"), "<template>{{ WEEK }}</template>")
            }
            "nested macro" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                caller.replace("beta::items::WEEK", "{ inject!(); beta::items::WEEK }"),
            ),
            "nested attribute" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                caller.replace(
                    "beta::items::WEEK",
                    "{ #[inject] struct Other; beta::items::WEEK }",
                ),
            ),
            "macro import" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                format!("use other::println;\n{caller}"),
            ),
            "private destination" => fs::write(repo.join("crates/beta/src/lib.rs"), "mod items;\n"),
            "custom target" => fs::write(
                repo.join("crates/beta/Cargo.toml"),
                format!("{}\n[lib]\npath = \"src/items.rs\"\n", package("beta", "")),
            ),
            "same package binary caller" => fs::write(
                repo.join("crates/beta/src/main.rs"),
                "fn main() { let _ = crate::items::WEEK; }\n",
            ),
            "binary destination in library package" => {
                fs::write(repo.join(BETA), "").unwrap();
                fs::write(
                    repo.join("crates/beta/src/main.rs"),
                    format!("{WEEK_AFTER}\nfn main() {{}}\n"),
                )
                .unwrap();
                fs::write(
                    repo.join("crates/consumer/src/main.rs"),
                    consumer("beta", &WEEK_USE.replace("::items", "")),
                )
            }
            "block module" => fs::write(
                repo.join("crates/beta/src/lib.rs"),
                "pub mod items;\nfn helper() { mod items; }\n",
            ),
            "macro block module" => fs::write(
                repo.join("crates/consumer/src/main.rs"),
                caller.replace("beta::items::WEEK", "{ mod items; beta::items::WEEK }"),
            ),
            _ => unreachable!(),
        }
        .unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert_kept_removed(&api, "WEEK", label);
    }
}
