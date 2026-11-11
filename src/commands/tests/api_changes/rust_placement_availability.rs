//! Rust の公開の項目の有る設定の比較に、ファイルを置く module の置き場の条件を足す (Issue #316)。

use crate::commands::tests::common::*;
use crate::models::review::{ApiChanges, ApiModificationUnverifiedReason, ApiSymbol};
use std::fs;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\ny = []\n";

/// `files` をコミットし、`changes` に書き換えた (中身が None なら消した) ときの API の差分。
fn api_changes(files: &[(&str, &str)], changes: &[(&str, Option<&str>)]) -> ApiChanges {
    api_changes_in(CARGO, files, changes)
}

/// [`api_changes`] の、crate root の manifest を `manifest` にしたもの (workspace など)。
fn api_changes_in(
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

/// 名前 `symbol` が `name` か、impl の対象の型で修飾した `name` (`S.make`) か。
fn named(symbol: &str, name: &str) -> bool {
    symbol == name
        || symbol
            .strip_suffix(name)
            .is_some_and(|owner| owner.ends_with('.'))
}

/// `name` の区分 (rm・rm_dead・add・mod・none。合わせて 2 回以上出れば dup)。
fn classify(api: &ApiChanges, name: &str) -> &'static str {
    let count = |symbols: &[ApiSymbol]| {
        symbols
            .iter()
            .filter(|symbol| named(&symbol.name, name))
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
                .filter(|change| named(&change.name, name))
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

/// 比べられなかったもの (blocking) として出たか。
fn unverified(api: &ApiChanges, name: &str) -> bool {
    api.modified.iter().any(|change| {
        named(&change.name, name)
            && change.unverified.as_ref().is_some_and(|unverified| {
                unverified.reason == ApiModificationUnverifiedReason::RustCfgAvailabilityUnverified
            })
    })
}

const MAKE: &str = "pub fn make() -> u32 {\n    1\n}\n";

/// 一意で確定した公開の置き場で既に要る条件は、項目に足しても外しても有る設定を変えない。
#[test]
fn redundant_item_conditions_under_proven_placements_do_not_change_availability() {
    let lib = ("src/lib.rs", "#[cfg(feature = \"x\")]\npub mod api;\n");
    for (old, new, expected) in [
        (
            MAKE.to_owned(),
            format!("#[cfg(feature = \"x\")]\n{MAKE}"),
            "none",
        ),
        (
            format!("#[cfg(feature = \"x\")]\n{MAKE}"),
            MAKE.to_owned(),
            "none",
        ),
        (
            MAKE.to_owned(),
            format!("#[cfg(all(feature = \"x\", feature = \"y\"))]\n{MAKE}"),
            "rm_dead",
        ),
    ] {
        let api = api_changes(&[lib, ("src/api.rs", &old)], &[("src/api.rs", Some(&new))]);
        assert_eq!(classify(&api, "make"), expected, "{old}\n{new}\n{api:?}");
    }
}

/// 配置が曖昧・非公開・マクロで変わり得るときは、項目自身の条件の変化を抑制しない。
#[test]
fn uncertain_placements_keep_item_condition_reports() {
    for (lib, expected) in [
        (
            "#[cfg(feature = \"x\")]\nmod api;\n#[cfg(feature = \"x\")]\npub use api::make;\n",
            "rm",
        ),
        (
            "#[cfg(feature = \"x\")]\npub mod api;\n#[path = \"api.rs\"]\npub mod other;\n",
            "rm_dead",
        ),
        (
            "#[cfg(feature = \"x\")]\npub mod api;\nmake_modules!();\n",
            "rm_dead",
        ),
        (
            "#[cfg(feature = \"x\")]\n#[replace_module]\npub mod api;\n",
            "rm_dead",
        ),
        (
            "#[cfg(feature = \"x\")]\npub mod api;\n#[cfg_attr(feature = \"y\", path = \"other.rs\")]\nmod unknown;\n",
            "rm_dead",
        ),
    ] {
        let api = api_changes(
            &[
                ("src/lib.rs", lib),
                ("src/api.rs", MAKE),
                ("src/other.rs", ""),
            ],
            &[(
                "src/api.rs",
                Some(&format!("#[cfg(feature = \"x\")]\n{MAKE}")),
            )],
        );
        assert_eq!(classify(&api, "make"), expected, "{lib}\n{api:?}");
    }
}

/// 祖先ファイルの内側の条件、inline module と cfg_attr の条件も配置の証明に含む。
#[test]
fn nested_placement_conditions_are_included_in_item_comparisons() {
    let api = api_changes(
        &[
            ("src/lib.rs", "#![cfg(feature = \"x\")]\npub mod outer;\n"),
            (
                "src/outer.rs",
                "#[cfg_attr(feature = \"y\", cfg(feature = \"x\"))]\npub mod inner;\n",
            ),
            ("src/outer/inner.rs", MAKE),
        ],
        &[(
            "src/outer/inner.rs",
            Some(&format!("#[cfg(feature = \"x\")]\n{MAKE}")),
        )],
    );
    assert_eq!(classify(&api, "make"), "none", "{api:?}");
}

/// Issue の再現 2: 項目を、条件の無い module から `x` の付いた module へ移すと、`x` を有効にしない設定から消える。
/// 同じ条件の module の間の移動は変わらず、条件の付いた module から条件の無い module への移動は増える。公開の経路の
/// 置き場だけを比べる (非公開の別の置き場は、公開の置き場で消える設定を覆わない)。
#[test]
fn moves_compare_the_placements_of_both_modules() {
    let g = "pub fn g() -> u32 {\n    2\n}\n";
    for (lib, expected) in [
        (
            "pub mod a;\n#[cfg(feature = \"x\")]\npub mod b;\n",
            "rm_dead",
        ),
        (
            "#[cfg(feature = \"x\")]\npub mod a;\n#[cfg(feature = \"x\")]\npub mod b;\n",
            "none",
        ),
        ("#[cfg(feature = \"x\")]\npub mod a;\npub mod b;\n", "add"),
        (
            "pub mod a;\n#[cfg(feature = \"x\")]\npub mod b;\n#[path = \"b.rs\"]\nmod hidden;\n",
            "rm_dead",
        ),
    ] {
        let api = api_changes(
            &[("src/lib.rs", lib), ("src/a.rs", MAKE), ("src/b.rs", g)],
            &[
                ("src/a.rs", Some("")),
                ("src/b.rs", Some(&format!("{g}\n{MAKE}"))),
            ],
        );
        assert_eq!(classify(&api, "make"), expected, "{lib}\n{api:?}");
    }
}

/// 非公開の module の間の移動は、外から使える条件を `pub use` の再公開が決めるので、置き場の条件では比べない
/// (再公開の条件を保てば報告しない)。同じファイルを 2 つの crate が置くときは、crate ごとに旧新を比べる。
#[test]
fn private_moves_and_crates_are_compared_on_their_own() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "mod a;\n#[cfg(feature = \"x\")]\nmod b;\n#[cfg(feature = \"x\")]\npub use a::make;\n",
            ),
            ("src/a.rs", MAKE),
            ("src/b.rs", ""),
        ],
        &[
            (
                "src/lib.rs",
                Some(
                    "mod a;\n#[cfg(feature = \"x\")]\nmod b;\n#[cfg(feature = \"x\")]\npub use b::make;\n",
                ),
            ),
            ("src/a.rs", Some("")),
            ("src/b.rs", Some(MAKE)),
        ],
    );
    assert_eq!(classify(&api, "make"), "none", "{api:?}");
    let package = |name: &str, features: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{features}")
    };
    let left = package("left", "\n[features]\nx = []\n");
    let right = package("right", "");
    let keep_a = "pub fn keep_a() {}\n";
    let keep_b = "pub fn keep_b() {}\n";
    let api = api_changes_in(
        "[workspace]\nmembers = [\"left\", \"right\"]\nresolver = \"3\"\n",
        &[
            ("left/Cargo.toml", &left),
            ("right/Cargo.toml", &right),
            (
                "left/src/lib.rs",
                "#[cfg(feature = \"x\")]\npub mod a;\n#[cfg(feature = \"x\")]\n#[path = \"../../right/src/b.rs\"]\npub mod b;\n",
            ),
            (
                "right/src/lib.rs",
                "#[path = \"../../left/src/a.rs\"]\npub mod a;\npub mod b;\n",
            ),
            ("left/src/a.rs", &format!("{keep_a}{MAKE}")),
            ("right/src/b.rs", keep_b),
        ],
        &[
            ("left/src/a.rs", Some(keep_a)),
            ("right/src/b.rs", Some(&format!("{keep_b}{MAKE}"))),
        ],
    );
    assert_eq!(classify(&api, "make"), "none", "{api:?}");
}

/// ファイルごと別の条件の module へ移す (内容を変える rename・内容の同じ rename) と、移動元の旧い置き場と移動先の
/// 新しい置き場の条件で比べる。
#[test]
fn renamed_files_compare_the_placements_of_both_modules() {
    for moved in [format!("// moved\n{MAKE}"), MAKE.to_owned()] {
        let api = api_changes(
            &[("src/lib.rs", "pub mod a;\n"), ("src/a.rs", MAKE)],
            &[
                ("src/lib.rs", Some("#[cfg(feature = \"x\")]\npub mod b;\n")),
                ("src/a.rs", None),
                ("src/b.rs", Some(&moved)),
            ],
        );
        assert_eq!(classify(&api, "make"), "rm_dead", "{moved}\n{api:?}");
    }
}

/// ほかの package のファイルを `#[path]` で読む module から、条件の付いた module へ移すときも、移動元の置き場で
/// 比べる (置き場は差分の中の全ての package の lib crate でたどり、旧新を同じ lib crate どうしで比べる)。
#[test]
fn files_read_from_other_packages_are_placed_by_their_crates() {
    let manifest = format!("{CARGO}\n[workspace]\nmembers = [\"inner\"]\n");
    let inner = (
        "inner/Cargo.toml",
        "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    let g = "pub fn g() -> u32 {\n    2\n}\n";
    let api = api_changes_in(
        &manifest,
        &[
            (
                "src/lib.rs",
                "#[path = \"../inner/src/shared.rs\"]\npub mod a;\n\n#[cfg(feature = \"x\")]\npub mod b;\n",
            ),
            ("src/b.rs", g),
            inner,
            ("inner/src/lib.rs", ""),
            ("inner/src/shared.rs", MAKE),
        ],
        &[
            ("inner/src/shared.rs", Some("")),
            ("src/b.rs", Some(&format!("{g}\n{MAKE}"))),
        ],
    );
    assert_eq!(classify(&api, "make"), "rm_dead", "{api:?}");
    // 移動元も移動先もほかの package の中のファイルで、置く crate は `src/lib.rs` の crate (内容の同じ rename)
    let api = api_changes_in(
        &manifest,
        &[
            (
                "src/lib.rs",
                "#[path = \"../inner/src/a.rs\"]\npub mod a;\n",
            ),
            inner,
            ("inner/src/lib.rs", ""),
            ("inner/src/a.rs", MAKE),
        ],
        &[
            (
                "src/lib.rs",
                Some("#[cfg(feature = \"x\")]\n#[path = \"../inner/src/b.rs\"]\npub mod b;\n"),
            ),
            ("inner/src/a.rs", None),
            ("inner/src/b.rs", Some(MAKE)),
        ],
    );
    assert_eq!(classify(&api, "make"), "rm_dead", "{api:?}");
}

/// 置き場を決められない (場所を決められない `mod` の宣言がある) まま別の module へ移すと、置き場の条件が違い得る
/// ので比べられなかったもの (blocking) にする。同じファイルの中の変更は今までどおり比べる。
#[test]
fn undetermined_placements_block_moves() {
    let lib = "pub mod a;\n#[cfg_attr(feature = \"y\", path = \"other.rs\")]\npub mod b;\n";
    let g = "pub fn g() -> u32 {\n    2\n}\n";
    let api = api_changes(
        &[
            ("src/lib.rs", lib),
            ("src/a.rs", MAKE),
            ("src/b.rs", g),
            ("src/other.rs", g),
        ],
        &[
            ("src/a.rs", Some("")),
            ("src/b.rs", Some(&format!("{g}\n{MAKE}"))),
        ],
    );
    assert!(unverified(&api, "make"), "{api:?}");
}

/// 置き場を決められないまま移しても、宣言自身の条件の比較が見つけた変化は残す (比べられなかったものにすると、
/// 旧い版で非公開の module にあった項目の増える設定を落とす)。
#[test]
fn undetermined_placements_keep_item_condition_reports() {
    let body = "pub fn make() -> u32 {\n    let first = 1;\n    let second = 2;\n    let third = 3;\n    first + second + third\n}\n";
    let api = api_changes(
        &[
            ("src/lib.rs", "mod a;\nmod unrelated;\n"),
            ("src/a.rs", &format!("#[cfg(feature = \"x\")]\n{body}")),
            ("src/unrelated.rs", ""),
            ("src/other.rs", ""),
        ],
        &[
            (
                "src/lib.rs",
                Some(
                    "pub mod b;\n#[cfg_attr(feature = \"y\", path = \"other.rs\")]\nmod unrelated;\n",
                ),
            ),
            ("src/a.rs", None),
            ("src/b.rs", Some(body)),
        ],
    );
    assert_eq!(classify(&api, "make"), "add", "{api:?}");
}

/// inherent impl の項目は、置いた module ではなく対象の型で外から使えるかが決まり、型が外から使える条件は設定と
/// 置き場で違い得るので、置き場の条件では比べない (外から使えない設定での減少を公開の項目の削除にしない。どの
/// 設定でも同じ型の項目が 1 つある移動を、経路ごとの変化にしない)。
#[test]
fn inherent_impl_members_are_not_compared_by_placements() {
    let impl_block = "impl crate::S {\n    pub fn make() -> u32 {\n        1\n    }\n}\n";
    let split_owner = "#[cfg(feature = \"x\")]\npub struct S;\n#[cfg(not(feature = \"x\"))]\nstruct S;\n\n#[cfg(feature = \"x\")]\npub fn call() -> u32 {\n    S::make()\n}\n";
    let a = format!("fn keep_a() {{}}\n{impl_block}");
    let b = format!("fn keep_b() {{}}\n{impl_block}");
    for files in [
        vec![(
            "src/lib.rs",
            format!("{split_owner}mod a;\n#[cfg(feature = \"x\")]\nmod b;\n"),
        )],
        vec![(
            "src/lib.rs",
            format!("{split_owner}pub mod a;\n#[cfg(feature = \"x\")]\npub mod b;\n"),
        )],
        vec![
            (
                "src/lib.rs",
                "pub struct S;\npub mod p;\npub mod q;\n\npub fn call() -> u32 {\n    S::make()\n}\n"
                    .to_owned(),
            ),
            (
                "src/p.rs",
                "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod a;\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod b;\n"
                    .to_owned(),
            ),
            (
                "src/q.rs",
                "#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod a;\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod b;\n"
                    .to_owned(),
            ),
        ],
    ] {
        let mut committed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, content)| (*path, content.as_str()))
            .collect();
        committed.extend([("src/a.rs", a.as_str()), ("src/b.rs", "fn keep_b() {}\n")]);
        let api = api_changes(
            &committed,
            &[
                ("src/a.rs", Some("fn keep_a() {}\n")),
                ("src/b.rs", Some(&b)),
            ],
        );
        assert_eq!(classify(&api, "make"), "none", "{files:?}\n{api:?}");
    }
}

/// 同じ module の経路に公開の宣言と非公開の宣言があれば、公開の宣言の条件だけを足す (`x` の無い設定では非公開の
/// module の項目で、外から使えない)。条件の無い公開の module へ移すと、`x` の無い設定に増える。
#[test]
fn private_declarations_of_the_same_module_do_not_count() {
    let g = "pub fn g() -> u32 {\n    2\n}\n";
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\npub mod a;\n#[cfg(not(feature = \"x\"))]\nmod a;\npub mod b;\n",
            ),
            ("src/a.rs", MAKE),
            ("src/b.rs", g),
        ],
        &[
            ("src/a.rs", Some("")),
            ("src/b.rs", Some(&format!("{g}\n{MAKE}"))),
        ],
    );
    assert_eq!(classify(&api, "make"), "add", "{api:?}");
}

/// 移動元の module の経路をほかのファイルも置く (cfg で分けた `#[path]`) と、経路の項目が移動した項目と同じとは
/// 言えないので、置き場の条件では比べない (`api::make` はほかのファイルで残る)。
#[test]
fn modules_other_files_also_place_are_not_compared() {
    let api = api_changes(
        &[
            (
                "src/lib.rs",
                "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod api;\n#[cfg(not(feature = \"x\"))]\n#[path = \"c.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"d.rs\"]\npub mod extra;\n\npub mod caller;\n",
            ),
            ("src/a.rs", MAKE),
            ("src/c.rs", MAKE),
            ("src/d.rs", ""),
            (
                "src/caller.rs",
                "pub fn call() -> u32 {\n    crate::api::make()\n}\n",
            ),
        ],
        &[
            (
                "src/lib.rs",
                Some(
                    "#[path = \"c.rs\"]\npub mod api;\n\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod extra;\n\npub mod caller;\n",
                ),
            ),
            ("src/a.rs", None),
            ("src/b.rs", Some(MAKE)),
        ],
    );
    let removed = api
        .removed
        .iter()
        .chain(&api.removed_dead)
        .any(|symbol| symbol.name == "make");
    assert!(!removed && !unverified(&api, "make"), "{api:?}");
}

/// lib の module のファイルを lib のどこにも置かれないファイルへ移す (rename) と、置き場の条件では比べない (lib の
/// 公開の項目からの出入りは module の比べ直しが扱うので、比べられなかったもの (blocking) にしない)。
#[test]
fn moves_out_of_the_lib_are_not_blocked() {
    let helpers: String = (0..12)
        .map(|index| format!("pub fn helper{index}() -> u32 {{\n    {index}\n}}\n\n"))
        .collect();
    let changed = helpers.replacen("    0\n", "    100\n", 1);
    let api = api_changes(
        &[
            ("src/lib.rs", "pub mod api;\n"),
            ("src/api.rs", &format!("{helpers}{MAKE}")),
        ],
        &[
            ("src/lib.rs", Some("")),
            ("src/api.rs", None),
            ("src/unused.rs", Some(&format!("{changed}{MAKE}"))),
        ],
    );
    assert!(!unverified(&api, "make"), "{api:?}");
}

/// 消える設定と増える設定を別々の lib crate で見つけたら、それぞれの crate の置き場の条件で表示する (片方の
/// crate の表示を両方に添えない)。
#[test]
fn changes_found_in_different_crates_show_their_own_placements() {
    let package = |name: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n"
        )
    };
    let (left, right) = (package("left"), package("right"));
    let keep_a = "pub fn keep_a() {}\n";
    let keep_b = "pub fn keep_b() {}\n";
    let api = api_changes_in(
        "[workspace]\nmembers = [\"left\", \"right\"]\nresolver = \"3\"\n",
        &[
            ("left/Cargo.toml", &left),
            ("right/Cargo.toml", &right),
            (
                "left/src/lib.rs",
                "pub mod a;\n#[cfg(feature = \"x\")]\n#[path = \"../../right/src/b.rs\"]\npub mod b;\n",
            ),
            (
                "right/src/lib.rs",
                "#[cfg(feature = \"x\")]\n#[path = \"../../left/src/a.rs\"]\npub mod a;\npub mod b;\n",
            ),
            ("left/src/a.rs", &format!("{keep_a}{MAKE}")),
            ("right/src/b.rs", keep_b),
        ],
        &[
            ("left/src/a.rs", Some(keep_a)),
            ("right/src/b.rs", Some(&format!("{keep_b}{MAKE}"))),
        ],
    );
    let shown = |symbols: &[ApiSymbol]| {
        symbols
            .iter()
            .find(|symbol| symbol.name == "make")
            .and_then(|symbol| symbol.cfg_availability.clone())
    };
    let x = |sides: &[Vec<String>]| {
        sides
            .iter()
            .flatten()
            .any(|condition| condition.contains("feature = \"x\""))
    };
    let removed = shown(&api.removed_dead)
        .or_else(|| shown(&api.removed))
        .unwrap_or_else(|| panic!("{api:?}"));
    let added = shown(&api.added).unwrap_or_else(|| panic!("{api:?}"));
    assert!(!x(&removed.old) && x(&removed.new), "{removed:?}\n{api:?}");
    assert!(x(&added.old) && !x(&added.new), "{added:?}\n{api:?}");
}
