//! Rust の Cargo の到達性による impact の除外と、標準のマクロ・derive (Issue #117)。

use super::support::*;

/// a の `Result` を変えたときの、b と c の呼び出し元の扱い (impacted か informational に出るか)。
/// c は a に依存し、b は依存しない。
fn callers_after_change(b_source: &str, dependencies: &str) -> (bool, bool, serde_json::Value) {
    let repo = TestRepo::new();
    for package in ["a", "b", "c"] {
        repo.create_dir_all(format!("{package}/src"));
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
    );
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"sample-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "b/Cargo.toml",
        format!("[package]\nname = \"sample-b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nanyhow = \"1\"\n{dependencies}"),
    );
    repo.write(
        "c/Cargo.toml",
        "[package]\nname = \"sample-c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nsample-a = { path = \"../a\" }\n",
    );
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, ()>;\n",
    );
    repo.write("b/src/lib.rs", b_source);
    repo.write(
        "c/src/lib.rs",
        "use sample_a::Result;\npub fn caller() -> Result<()> {\n    println!(\"caller\");\n    Ok(())\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, u8>;\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "a/src/lib.rs")
        .unwrap()
        .clone();
    let lists_path = |key: &str, path: &str| {
        change[key]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == path))
    };
    let b_listed = lists_path("impacted_callers", "b/src/lib.rs")
        || lists_path("informational_callers", "b/src/lib.rs");
    let c_impacted = lists_path("impacted_callers", "c/src/lib.rs");
    (b_listed, c_impacted, context)
}

#[test]
fn impact_rust_cargo_reachability_allows_standard_macros_and_derives() {
    // b に標準のマクロと derive があっても、a に依存しない b の同名の型は impact に数えない。
    let (b_listed, c_impacted, context) = callers_after_change(
        "use anyhow::Result;\n\n#[derive(Debug)]\npub struct Marker;\n\npub fn unrelated() -> Result<()> {\n    println!(\"unrelated\");\n    Ok(())\n}\n",
        "",
    );
    assert!(!b_listed && c_impacted, "{context}");
}

#[test]
fn impact_rust_cargo_reachability_keeps_callers_for_unknown_expansions() {
    // 標準のマクロを上書きしうる外部の glob や、外部のマクロがあれば到達性で除外しない。
    for b_source in [
        "use anyhow::*;\n\npub fn unrelated() -> Result<()> {\n    println!(\"unrelated\");\n    Ok(())\n}\n",
        "use anyhow::Result;\n\npub fn unrelated() -> Result<()> {\n    anyhow::ensure!(true, \"x\");\n    Ok(())\n}\n",
    ] {
        let (b_listed, c_impacted, context) = callers_after_change(b_source, "");
        assert!(b_listed && c_impacted, "{b_source}: {context}");
    }
}

#[test]
fn impact_rust_cargo_reachability_checks_macro_arguments_and_glob_aliases() {
    let (b_listed, c_impacted, context) = callers_after_change(
        "use anyhow::Result;\npub fn unrelated() -> Result<()> {\n    let _ = vec![{ let value = 1; assert_eq!(value, 1); value }];\n    Ok(())\n}\n#[cfg(test)]\nmod tests { use super::*; #[test] fn works() { assert!(unrelated().is_ok()); } }\n",
        "",
    );
    assert!(!b_listed && c_impacted, "{context}");
    for source in [
        "use anyhow::Result;\npub use custom as origin;\npub use crate::origin as relay;\nuse crate::relay::*;\npub fn unrelated() -> Result<()> { println!(\"x\"); Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ use custom::println; println!(); 1 }]; Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ #[custom::attribute] fn generated() {} 1 }]; Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ #[derive(custom::Derive)] struct Generated; 1 }]; Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ macro_rules! println { () => {}; } println!(); 1 }]; Ok(()) }\n",
    ] {
        let (b_listed, c_impacted, context) = callers_after_change(source, "custom = \"1\"\n");
        assert!(b_listed && c_impacted, "{source}: {context}");
    }
    for name in ["proc_macro", "test"] {
        let dependencies = format!("{name} = {{ package = \"custom\", version = \"1\" }}\n");
        let source = format!(
            "extern crate {name} as shim;\nuse crate::shim::*;\nuse anyhow::Result;\npub fn unrelated() -> Result<()> {{ println!(\"x\"); Ok(()) }}\n"
        );
        let (b_listed, c_impacted, context) = callers_after_change(&source, &dependencies);
        assert!(b_listed && c_impacted, "{source}: {context}");
    }
}

/// Issue #373 の最小再現。bin だけの app の中で `Page` を workspace.rs から state.rs へ移し、
/// 変更元 state.rs の呼び出し元の path を (impacted, 他のバケット) で返す。`app_targets` は
/// app の manifest の target の節。fetch-lib は core-lib だけに依存し、core-lib の enum の同名の
/// variant (`PagesSpec::Page`・`Placeholder::Page`) を使う。
fn moved_page_in_binary_package(app_targets: &str) -> (Vec<String>, Vec<String>, String) {
    let repo = TestRepo::new();
    for directory in [
        "crates/core-lib/src",
        "crates/fetch-lib/src",
        "crates/app/src",
    ] {
        repo.create_dir_all(directory);
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/core-lib\", \"crates/fetch-lib\", \"crates/app\"]\nresolver = \"3\"\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace.dependencies]\ncore-lib = { path = \"crates/core-lib\" }\nfetch-lib = { path = \"crates/fetch-lib\" }\n",
    );
    repo.write(
        "crates/core-lib/Cargo.toml",
        "[package]\nname = \"core-lib\"\nversion.workspace = true\nedition.workspace = true\n",
    );
    repo.write("crates/core-lib/src/lib.rs", "pub mod source;\n");
    repo.write(
        "crates/core-lib/src/source.rs",
        "pub struct PageNumbers {\n    pub max_pages: u32,\n}\n\npub enum PagesSpec {\n    Page(PageNumbers),\n    NextLink,\n}\n\nimpl PagesSpec {\n    pub fn max_pages(&self) -> u32 {\n        match self {\n            PagesSpec::Page(pages) => pages.max_pages,\n            PagesSpec::NextLink => 1,\n        }\n    }\n}\n\npub enum Placeholder {\n    Var(String),\n    Page,\n}\n",
    );
    repo.write(
        "crates/fetch-lib/Cargo.toml",
        "[package]\nname = \"fetch-lib\"\nversion.workspace = true\nedition.workspace = true\n\n[dependencies]\ncore-lib.workspace = true\n",
    );
    repo.write(
        "crates/fetch-lib/src/lib.rs",
        "pub mod engine;\nmod template;\n",
    );
    repo.write(
        "crates/fetch-lib/src/engine.rs",
        "use core_lib::source::PagesSpec;\n\nstruct Page {\n    items: Vec<u32>,\n}\n\npub fn has_next(spec: Option<&PagesSpec>) -> bool {\n    matches!(spec, Some(PagesSpec::Page(_)))\n}\n\npub fn count() -> usize {\n    read_page().map(|page| page.items.len()).unwrap_or(0)\n}\n\nfn read_page() -> Result<Page, String> {\n    Ok(Page { items: Vec::new() })\n}\n",
    );
    repo.write(
        "crates/fetch-lib/src/template.rs",
        "use core_lib::source::Placeholder;\n\npub fn name(placeholder: &Placeholder) -> String {\n    match placeholder {\n        Placeholder::Var(name) => name.clone(),\n        Placeholder::Page => \"page\".into(),\n    }\n}\n",
    );
    repo.write("crates/fetch-lib/src/tool.rs", "fn main() {}\n");
    repo.write(
        "crates/app/Cargo.toml",
        format!("[package]\nname = \"app\"\nversion.workspace = true\nedition.workspace = true\n\n{app_targets}\n[dependencies]\ncore-lib.workspace = true\nfetch-lib.workspace = true\n"),
    );
    repo.write(
        "crates/app/src/main.rs",
        "mod state;\nmod workspace;\n\nfn main() {\n    let page = workspace::Page::News;\n    let state = state::State::default();\n    println!(\"{:?} {} {}\", page, state.count, fetch_lib::engine::count());\n}\n",
    );
    repo.write(
        "crates/app/src/state.rs",
        "use std::collections::HashMap;\n\n#[derive(Default)]\npub struct State {\n    pub count: usize,\n    pub names: HashMap<u32, String>,\n}\n",
    );
    repo.write(
        "crates/app/src/workspace.rs",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Page {\n    News,\n    Settings,\n}\n\npub fn is_news(page: Page) -> bool {\n    page == Page::News\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "crates/app/src/state.rs",
        "use std::collections::{HashMap, HashSet};\n\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Page {\n    News,\n    Settings,\n}\n\n#[derive(Default)]\npub struct State {\n    pub count: usize,\n    pub names: HashMap<u32, String>,\n    pub seen: HashSet<u32>,\n}\n",
    );
    repo.write(
        "crates/app/src/workspace.rs",
        "pub use crate::state::Page;\n\npub fn is_news(page: Page) -> bool {\n    page == Page::News\n}\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "crates/app/src/state.rs")
        .unwrap_or_else(|| panic!("{context}"));
    let paths = |keys: &[&str]| {
        let mut paths: Vec<String> = keys
            .iter()
            .filter_map(|key| change[*key].as_array())
            .flatten()
            .map(|caller| caller["path"].as_str().unwrap().to_owned())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    };
    (
        paths(&["impacted_callers"]),
        paths(&["informational_callers", "low_confidence_callers"]),
        context.to_string(),
    )
}

#[test]
fn impact_rust_cargo_reachability_accepts_target_paths_inside_the_package() {
    // bin だけの app の `path = "src/main.rs"` で証明を止めず、app に依存しない fetch-lib の
    // 同名の variant を出さない。再公開を通した main.rs の参照は残す (Issue #373)。
    let (impacted, others, context) =
        moved_page_in_binary_package("[[bin]]\nname = \"app-bin\"\npath = \"src/main.rs\"\n");
    assert_eq!(impacted, ["crates/app/src/main.rs"], "{context}");
    assert!(
        others.iter().all(|path| path.starts_with("crates/app/")),
        "{context}"
    );
}

#[test]
fn impact_rust_cargo_reachability_keeps_callers_for_target_paths_outside_the_package() {
    // app の target が fetch-lib のソースをコンパイルするなら、fetch-lib のファイルも app の依存の
    // 名前で解決されうるので、到達性で外さない。
    let (impacted, _, context) = moved_page_in_binary_package(
        "[[bin]]\nname = \"app-bin\"\npath = \"src/main.rs\"\n\n[[bin]]\nname = \"fetch-tool\"\npath = \"../fetch-lib/src/tool.rs\"\n",
    );
    assert_eq!(
        impacted,
        [
            "crates/app/src/main.rs",
            "crates/fetch-lib/src/engine.rs",
            "crates/fetch-lib/src/template.rs"
        ],
        "{context}"
    );
}

/// a の `run` に引数を足し、同時に `rename` を `git mv` したときの、a の呼び出し元の path。
/// `edit` なら移した先の末尾に 1 行足す (内容も変えた rename)。b だけが a に依存し、
/// `b/src/bin/probe.rs` が `a::run(1)` を呼ぶ。c は a に依存せず、外部の `other` から import した
/// 同名の `run` を呼ぶ (到達性だけが外せる)。
fn callers_with_hunkless_rename(rename: Option<(&str, &str)>, edit: bool) -> (Vec<String>, String) {
    let repo = TestRepo::new();
    for directory in ["a/src", "b/src/bin", "c/src", "notes"] {
        repo.create_dir_all(directory);
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
    );
    let package = |name: &str, dependencies: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{dependencies}"
        )
    };
    repo.write("a/Cargo.toml", package("a", ""));
    repo.write(
        "b/Cargo.toml",
        package("b", "[dependencies]\na = { path = \"../a\" }\n"),
    );
    repo.write(
        "c/Cargo.toml",
        package("c", "[dependencies]\nother = \"1\"\n"),
    );
    repo.write("a/src/lib.rs", "pub fn run(x: u8) -> u8 {\n    x\n}\n");
    repo.write("b/src/lib.rs", "");
    repo.write("b/src/bin/probe.rs", "fn main() {\n    a::run(1);\n}\n");
    repo.write(
        "c/src/lib.rs",
        "use other::run;\n\npub fn caller() -> u8 {\n    run(1)\n}\n",
    );
    repo.write("notes/a.md", "line 1\nline 2\nline 3\nline 4\nline 5\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "a/src/lib.rs",
        "pub fn run(x: u8, extra: u8) -> u8 {\n    x + extra\n}\n",
    );
    if let Some((from, to)) = rename {
        repo.create_dir_all(std::path::Path::new(to).parent().unwrap());
        repo.git(["mv", from, to]);
        if edit {
            let mut content = std::fs::read(repo.path(to)).unwrap();
            content.extend_from_slice(b"line 6\n");
            repo.write(to, content);
        }
    }
    let context = repo.run_json("context", &["--git"]);
    let mut impacted: Vec<String> = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| change["path"] == "a/src/lib.rs")
        .filter_map(|change| change["impacted_callers"].as_array())
        .flatten()
        .map(|caller| caller["path"].as_str().unwrap().to_owned())
        .collect();
    impacted.sort();
    impacted.dedup();
    (impacted, context.to_string())
}

#[test]
fn impact_rust_cargo_reachability_keeps_callers_moved_by_hunkless_rename() {
    // 対照: rename が無い・Rust でないファイルを内容も変えて rename した (mode が diff に出る)
    // なら、到達性で c の同名の参照を外す。
    for (rename, edit) in [(None, false), (Some(("notes/a.md", "notes/b.md")), true)] {
        let (impacted, context) = callers_with_hunkless_rename(rename, edit);
        assert_eq!(impacted, ["b/src/bin/probe.rs"], "{rename:?}: {context}");
    }
    // 内容が同じまま c へ移した、変更前に b から a を呼んでいたコードは残す (Issue #374)。
    // Rust のソースの置き場所が変わったので、作業ツリーの依存のグラフで外さない。
    let (impacted, context) =
        callers_with_hunkless_rename(Some(("b/src/bin/probe.rs", "c/src/bin/probe.rs")), false);
    assert_eq!(
        impacted,
        ["c/src/bin/probe.rs", "c/src/lib.rs"],
        "{context}"
    );
    // 内容が同じ rename は diff に mode が出ず、gitlink の rename と区別できないので、Rust で
    // ないファイルでも到達性を使わない (Issue #377)。
    let (impacted, context) =
        callers_with_hunkless_rename(Some(("notes/a.md", "notes/b.md")), false);
    assert_eq!(
        impacted,
        ["b/src/bin/probe.rs", "c/src/lib.rs"],
        "{context}"
    );
}

/// b だけが a に依存し、`b/src/lib.rs` が `a::run(1)` を呼ぶワークスペース (c は a に依存せず、
/// 外部の `other` の同名の `run` を呼ぶ) で、a の `run` に引数を足したときの a の呼び出し元の path。
/// `prepare` はコミットの前に、`change` は a を変えた後に呼ぶ。
fn callers_of_changed_run(
    prepare: impl FnOnce(&TestRepo),
    change: impl FnOnce(&TestRepo),
) -> (Vec<String>, String) {
    let repo = TestRepo::new();
    for directory in ["a/src", "c/src"] {
        repo.create_dir_all(directory);
    }
    let package = |name: &str, dependencies: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{dependencies}"
        )
    };
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
    );
    repo.write("a/Cargo.toml", package("a", ""));
    repo.write(
        "c/Cargo.toml",
        package("c", "[dependencies]\nother = \"1\"\n"),
    );
    repo.write("a/src/lib.rs", "pub fn run(x: u8) -> u8 {\n    x\n}\n");
    repo.write(
        "c/src/lib.rs",
        "use other::run;\n\npub fn caller() -> u8 {\n    run(1)\n}\n",
    );
    repo.init_git();
    prepare(&repo);
    repo.commit_all("initial");
    repo.write(
        "a/src/lib.rs",
        "pub fn run(x: u8, extra: u8) -> u8 {\n    x + extra\n}\n",
    );
    change(&repo);
    let context = repo.run_json("context", &["--git"]);
    let mut impacted: Vec<String> = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| change["path"] == "a/src/lib.rs")
        .filter_map(|change| change["impacted_callers"].as_array())
        .flatten()
        .map(|caller| caller["path"].as_str().unwrap().to_owned())
        .collect();
    impacted.sort();
    impacted.dedup();
    (impacted, context.to_string())
}

const B_DEPENDS_ON_A: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\na = { path = \"../a\" }\n";
const B_WITHOUT_A: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const B_CALLER: &str = "pub fn caller() -> u8 {\n    a::run(1)\n}\n";

#[test]
fn impact_rust_cargo_reachability_keeps_callers_for_binary_manifest_changes() {
    let package_b = |repo: &TestRepo| {
        repo.create_dir_all("b/src");
        repo.write("b/Cargo.toml", B_DEPENDS_ON_A);
        repo.write("b/src/lib.rs", B_CALLER);
    };
    // 対照: Rust と Cargo に関係しない binary の変更では、到達性で c の同名の参照を外す。
    let (impacted, context) = callers_of_changed_run(
        |repo| {
            package_b(repo);
            repo.create_dir_all("assets");
            repo.write("assets/logo.bin", b"\x00\x01\x02");
        },
        |repo| repo.write("assets/logo.bin", b"\x00\x01\x03"),
    );
    assert_eq!(impacted, ["b/src/lib.rs"], "{context}");
    // `-diff` で binary になる manifest から a への依存を消しても、変更前の b は a を呼べた
    // ので外さない (Issue #377)。
    let (impacted, context) = callers_of_changed_run(
        |repo| {
            package_b(repo);
            repo.write(".gitattributes", "b/Cargo.toml -diff\n");
        },
        |repo| repo.write("b/Cargo.toml", B_WITHOUT_A),
    );
    assert_eq!(impacted, ["b/src/lib.rs", "c/src/lib.rs"], "{context}");
}

#[test]
fn impact_rust_cargo_reachability_keeps_callers_for_submodule_manifest_changes() {
    // b は submodule。submodule の中で b の manifest から a への依存を消し、親で gitlink を
    // 更新しても、変更前の b は a を呼べたので外さない。利用者の `diff.ignoreSubmodules` で
    // gitlink の更新を隠さない (Issue #377)。
    for ignore in [false, true] {
        let upstream = TestRepo::new();
        upstream.create_dir_all("src");
        upstream.write("Cargo.toml", B_DEPENDS_ON_A);
        upstream.write("src/lib.rs", B_CALLER);
        upstream.init_git();
        upstream.commit_all("b");
        let upstream_path = upstream.root().to_str().unwrap().to_owned();
        let (impacted, context) = callers_of_changed_run(
            |repo| {
                repo.git([
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "-q",
                    &upstream_path,
                    "b",
                ]);
                if ignore {
                    repo.git(["config", "diff.ignoreSubmodules", "all"]);
                }
            },
            |repo| {
                repo.write("b/Cargo.toml", B_WITHOUT_A);
                repo.git(["-C", "b", "config", "user.email", "astro-sight@example.com"]);
                repo.git(["-C", "b", "config", "user.name", "astro-sight"]);
                repo.git(["-C", "b", "commit", "-q", "-am", "drop a"]);
                repo.git(["add", "b"]);
            },
        );
        assert_eq!(
            impacted,
            ["b/src/lib.rs", "c/src/lib.rs"],
            "ignore={ignore}: {context}"
        );
    }
}

/// `target` を指す symlink (mode 120000) を、作業ツリーに作らずに index へ入れる。
fn add_symlink_to_index(repo: &TestRepo, path: &str, target: &str) {
    let mut child = std::process::Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo.root())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run git hash-object");
    std::io::Write::write_all(
        &mut child.stdin.take().expect("missing git stdin"),
        target.as_bytes(),
    )
    .expect("failed to write symlink target");
    let output = child
        .wait_with_output()
        .expect("failed to wait for git hash-object");
    assert!(output.status.success(), "git hash-object failed");
    let blob = String::from_utf8(output.stdout).unwrap();
    repo.git([
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("120000,{},{path}", blob.trim()),
    ]);
}

#[test]
fn impact_rust_cargo_reachability_is_not_used_inside_the_staged_snapshot() {
    // b の src/shared.rs は c の src/shared.rs への symlink で、c のファイルは b の module として
    // b の依存 a を呼ぶ。staged の snapshot は symlink を書き出さないので、c が a に依存しない
    // ことで外さない。review --staged を context --staged とそろえる (Issue #375)。
    let repo = TestRepo::new();
    for directory in ["a/src", "b/src", "c/src"] {
        repo.create_dir_all(directory);
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
    );
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\na = { path = \"../a\" }\n",
    );
    repo.write(
        "c/Cargo.toml",
        "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("a/src/lib.rs", "pub fn run(x: u8) -> u8 {\n    x\n}\n");
    repo.write("b/src/lib.rs", "mod shared;\n\npub use shared::call;\n");
    repo.write("c/src/lib.rs", "pub fn unrelated() {}\n");
    repo.write(
        "c/src/shared.rs",
        "pub fn call() -> u8 {\n    a::run(1)\n}\n",
    );
    repo.init_git();
    repo.stage_all();
    add_symlink_to_index(&repo, "b/src/shared.rs", "../../c/src/shared.rs");
    repo.git(["commit", "-q", "-m", "initial"]);
    repo.write(
        "a/src/lib.rs",
        "pub fn run(x: u8, extra: u8) -> u8 {\n    x + extra\n}\n",
    );
    repo.git(["add", "a/src/lib.rs"]);
    let lists_shared = |changes: &serde_json::Value| {
        changes.as_array().is_some_and(|changes| {
            changes.iter().any(|change| {
                change["path"] == "a/src/lib.rs"
                    && change["impacted_callers"]
                        .as_array()
                        .is_some_and(|callers| {
                            callers
                                .iter()
                                .any(|caller| caller["path"] == "c/src/shared.rs")
                        })
            })
        })
    };
    // 対照: context --staged は index の側を読み、到達性を使わない。
    let context = repo.run_json("context", &["--git", "--staged"]);
    assert!(lists_shared(&context["changes"]), "{context}");
    let review = repo.run_json("review", &["--git", "--staged"]);
    assert!(lists_shared(&review["impact"]["changes"]), "{review}");
}

#[test]
fn impact_rust_cargo_reachability_rejects_replaced_sysroot_in_legacy_tables() {
    let source = "use std::println;\nuse anyhow::Result;\npub fn unrelated() -> Result<()> { println!(\"x\"); Ok(()) }\n";
    let (b_listed, c_impacted, context) = callers_after_change(source, "");
    assert!(!b_listed && c_impacted, "{context}");
    for table in ["dev_dependencies", "build_dependencies"] {
        for prefix in ["", "target.'cfg(unix)'."] {
            let dependencies =
                format!("[{prefix}{table}]\nstd = {{ package = \"custom\", version = \"1\" }}\n");
            let (b_listed, c_impacted, context) = callers_after_change(source, &dependencies);
            assert!(b_listed && c_impacted, "{dependencies}: {context}");
        }
    }
    let (b_listed, c_impacted, context) = callers_after_change(
        "use std::include as println;\nprintln!(\"../../a/src/lib.rs\");\npub fn unrelated() -> Result { Result }\n",
        "",
    );
    // include! が作る型は解決しないが、到達性によって候補から消してはいけない。
    let b_low_confidence = context["changes"][0]["low_confidence_callers"]
        .as_array()
        .is_some_and(|callers| {
            callers
                .iter()
                .any(|caller| caller["path"] == "b/src/lib.rs")
        });
    assert!((b_listed || b_low_confidence) && c_impacted, "{context}");
}
