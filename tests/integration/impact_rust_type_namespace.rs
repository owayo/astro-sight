//! 非公開型の変更が届く Rust モジュールを確かめる (Issue #470)。

use std::collections::BTreeSet;

use super::support::*;

const ADMIN_OLD: &str = "use super::*;\npub(super) struct Administration {\n    settings: Mutex<config::Config>,\n}\nimpl Administration {\n    pub(super) fn current(&self) -> bool {\n        self.settings.lock().unwrap().learning\n    }\n}\n";
/// 非公開の `struct Settings` と `impl Settings` を、ほかの行の書き換えと同じ hunk で足す。
const ADMIN_NEW: &str = "use super::*;\npub(super) struct Administration {\n    settings: Mutex<Settings>,\n}\n/// The settings in force.\nstruct Settings {\n    config: config::Config,\n    unreadable: bool,\n}\nimpl Settings {\n    fn readable(&self) -> bool {\n        !self.unreadable\n    }\n}\nimpl Administration {\n    pub(super) fn current(&self) -> bool {\n        let current = self.settings.lock().unwrap();\n        current.readable() && current.config.learning\n    }\n}\n";

/// 同じ名前の型 `Settings` と列挙子 `Choice::Settings` を持つ、engine に依存しないクレートのファイル。
/// 先頭の glob と直下のマクロの呼び出しは、変更元の宣言を持ち込まない。
const BAR: &str = "use super::*;\nmacro_rules! noop {\n    () => {};\n}\nnoop!();\n#[derive(Clone, Copy, PartialEq)]\npub(super) enum Choice {\n    Direct,\n    Settings,\n}\n/// How the button reaches the settings.\npub(super) struct Settings {\n    pub(super) visible: bool,\n}\nimpl Settings {\n    pub(super) fn of_host(visible: bool) -> Self {\n        Settings { visible }\n    }\n    pub(super) fn again() -> Self {\n        Settings::of_host(true)\n    }\n}\npub(super) fn pick(settings: &Settings) -> Option<Choice> {\n    settings.visible.then_some(Choice::Settings)\n}\npub(super) fn label(choice: Choice) -> &'static str {\n    match choice {\n        Choice::Settings => \"settings\",\n        Choice::Direct => \"direct\",\n    }\n}\n";
const TIP_LIB: &str = "mod bar;\npub fn show() -> bool {\n    let chosen = bar::pick(&bar::Settings::of_host(true));\n    chosen.is_some() && ipc::ping()\n}\n";
const IPC_LIB: &str = "pub fn ping() -> bool {\n    true\n}\n";
/// engine に依存するクレートの、同じ名前の公開の型。
const UI_LIB: &str = "mod panel;\npub struct Settings {\n    pub dark: bool,\n}\npub fn render(settings: &Settings) -> bool {\n    panel::draw(settings) && ipc::ping()\n}\n";
/// glob で親の `Settings` を見るだけのファイル。参照ファイルの中に宣言が無い。
const PANEL: &str =
    "use super::*;\npub(super) fn draw(settings: &Settings) -> bool {\n    settings.dark\n}\n";

/// ipc・engine・tip・ui のワークスペース。members を glob で書き、Cargo の到達性の証明を使わない形に
/// する (tip は engine に依存しない。ui は依存する)。
fn workspace(admin: &str, bar: &str) -> TestRepo {
    let repo = TestRepo::new();
    for (package, dependencies) in [
        ("ipc", ""),
        ("engine", "ipc = { path = \"../ipc\" }\n"),
        ("tip", "ipc = { path = \"../ipc\" }\n"),
        (
            "ui",
            "ipc = { path = \"../ipc\" }\nengine = { path = \"../engine\" }\n",
        ),
    ] {
        repo.create_dir_all(format!("crates/{package}/src"));
        repo.write(
            format!("crates/{package}/Cargo.toml"),
            format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{dependencies}"),
        );
    }
    repo.create_dir_all("crates/engine/src/server");
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    repo.write("crates/ipc/src/lib.rs", IPC_LIB);
    repo.write(
        "crates/engine/src/lib.rs",
        "pub mod config;\npub mod server;\n",
    );
    repo.write(
        "crates/engine/src/config.rs",
        "#[derive(Clone, Default)]\npub struct Config {\n    pub learning: bool,\n}\n",
    );
    repo.write(
        "crates/engine/src/server.rs",
        "use crate::config;\nmod admin;\nuse std::sync::Mutex;\nfn serve(admin: &admin::Administration) -> bool {\n    admin.current()\n}\n",
    );
    repo.write("crates/engine/src/server/admin.rs", admin);
    repo.write("crates/tip/src/lib.rs", TIP_LIB);
    repo.write("crates/tip/src/bar.rs", bar);
    repo.write("crates/ui/src/lib.rs", UI_LIB);
    repo.write("crates/ui/src/panel.rs", PANEL);
    repo.init_git();
    repo.commit_all("initial");
    repo
}

/// `admin.rs` を `admin_new` に変えたときの、`Settings` の参照の行 (`パス: 行の本文`)。blocking と
/// low・informational の全部の一覧を合わせて返す。
fn settings_references(repo: &TestRepo, admin_new: &str) -> BTreeSet<String> {
    repo.write("crates/engine/src/server/admin.rs", admin_new);
    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "crates/engine/src/server/admin.rs")
        .unwrap_or_else(|| panic!("admin.rs is not analyzed: {context}"));
    let mut references = BTreeSet::new();
    for key in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        for caller in change[key].as_array().into_iter().flatten() {
            let path = caller["path"].as_str().unwrap();
            if path == "crates/engine/src/server/admin.rs"
                || !caller["symbols"]
                    .as_array()
                    .is_some_and(|symbols| symbols.iter().any(|name| name == "Settings"))
            {
                continue;
            }
            references.insert(format!("{path}: {}", repo.caller_source_line(caller)));
        }
    }
    references
}

#[test]
fn impact_rust_private_type_excludes_unrelated_and_dependent_crates() {
    let repo = workspace(ADMIN_OLD, BAR);
    let references = settings_references(&repo, ADMIN_NEW);
    assert!(references.is_empty(), "{references:?}");
}

#[test]
fn impact_rust_private_type_keeps_descendants_and_old_public_bindings() {
    let old = format!("mod child;\n{ADMIN_OLD}");
    let repo = workspace(&old, BAR);
    repo.create_dir_all("crates/engine/src/server/admin");
    repo.write(
        "crates/engine/src/server/admin/child.rs",
        "use super::*;\nfn take(_: &Settings) {}\n",
    );
    repo.commit_all("child module");
    let references = settings_references(&repo, &format!("mod child;\n{ADMIN_NEW}"));
    assert_eq!(
        references,
        BTreeSet::from([
            "crates/engine/src/server/admin/child.rs: fn take(_: &Settings) {}".to_owned()
        ])
    );

    for old in [
        format!("pub use crate::config::Config as Settings;\n{ADMIN_OLD}"),
        format!("pub struct Settings {{ pub other: bool }}\n{ADMIN_OLD}"),
    ] {
        let repo = workspace(&old, BAR);
        assert!(
            settings_references(&repo, ADMIN_NEW)
                .iter()
                .any(|line| line.contains("ui/src/panel.rs"))
        );
    }
}

#[test]
fn impact_rust_private_type_keeps_alternate_module_placements() {
    let repo = workspace(ADMIN_OLD, BAR);
    repo.write(
        "crates/engine/src/server/admin.rs",
        format!("#[path = \"../../../ui/src/panel.rs\"]\nmod alternate;\n{ADMIN_OLD}"),
    );
    repo.commit_all("shared module");
    let references = settings_references(
        &repo,
        &format!("#[path = \"../../../ui/src/panel.rs\"]\nmod alternate;\n{ADMIN_NEW}"),
    );
    assert!(
        references
            .iter()
            .any(|line| line.contains("ui/src/panel.rs")),
        "{references:?}"
    );
}

#[test]
fn impact_rust_private_type_keeps_names_expanded_in_the_child() {
    let repo = workspace(ADMIN_OLD, BAR);
    repo.write(
        "crates/engine/src/server.rs",
        "use crate::config;\nuse std::sync::Mutex;\nmacro_rules! settings_type {\n    () => { Settings };\n}\nmod admin;\n",
    );
    repo.commit_all("macro in parent module");
    let references = settings_references(
        &repo,
        &format!("{ADMIN_NEW}fn read(_: &settings_type!()) {{}}\n"),
    );
    assert!(
        references.contains("crates/engine/src/server.rs: () => { Settings };"),
        "{references:?}"
    );
}

#[test]
fn impact_rust_private_type_excludes_sibling_but_keeps_descendant() {
    let old = format!("mod child;\n{ADMIN_OLD}");
    let repo = workspace(&old, BAR);
    repo.create_dir_all("crates/engine/src/server/admin");
    repo.write(
        "crates/engine/src/server/admin/child.rs",
        "use super::*;\nfn child(_: &Settings) {}\n",
    );
    repo.write(
        "crates/engine/src/lib.rs",
        "pub mod config;\npub mod server;\nmod sidebar;\nstruct Settings;\n",
    );
    repo.write(
        "crates/engine/src/sidebar.rs",
        "use super::*;\nfn sibling(_: &Settings) {}\n",
    );
    repo.commit_all("sibling and descendant");
    let references = settings_references(&repo, &format!("mod child;\n{ADMIN_NEW}"));
    assert_eq!(
        references,
        BTreeSet::from([
            "crates/engine/src/server/admin/child.rs: fn child(_: &Settings) {}".to_owned()
        ])
    );
}

#[test]
fn impact_rust_private_type_literal_include_limits_uncertain_placement() {
    let repo = workspace(ADMIN_OLD, BAR);
    repo.write(
        "crates/ui/src/lib.rs",
        "mod panel;\nmod copy { include!(\"panel.rs\"); }\n",
    );
    repo.commit_all("literal include");
    let references = settings_references(&repo, ADMIN_NEW);
    assert!(
        references
            .iter()
            .any(|line| line.contains("ui/src/panel.rs")),
        "{references:?}"
    );
    assert!(
        !references.iter().any(|line| line.contains("tip/")),
        "{references:?}"
    );
    repo.write("crates/engine/src/server/admin.rs", ADMIN_OLD);
    repo.write("crates/ui/src/panel.rs", format!("mod nested;\n{PANEL}"));
    repo.commit_all("included external module");
    assert!(
        settings_references(&repo, ADMIN_NEW)
            .iter()
            .any(|line| line.contains("tip/"))
    );
}

/// 差分のほかのファイルは、通常の module の自由関数の本体 (マクロも入れ子の項目も無い) だけを
/// 変えたときに限って、非公開型の除外を保つ。メソッドの本体・マクロを含む本体・本体の外の変更が
/// あれば、参照を残す。
#[test]
fn impact_rust_private_type_gates_other_changed_files() {
    let ipc = "pub fn ping() -> bool {\n    true\n}\npub struct Link;\nimpl Link {\n    pub fn up(&self) -> bool {\n        true\n    }\n}\n";
    for (ipc_new, excluded) in [
        (ipc.replacen("    true\n", "    !false\n", 1), true),
        (ipc.replace("        true\n", "        !false\n"), false),
        (
            ipc.replacen("    true\n", "    assert!(true);\n    true\n", 1),
            false,
        ),
        (format!("{ipc}pub fn pong() {{}}\n"), false),
    ] {
        let repo = workspace(ADMIN_OLD, BAR);
        repo.write("crates/ipc/src/lib.rs", ipc);
        repo.commit_all("ipc link");
        repo.write("crates/ipc/src/lib.rs", &ipc_new);
        let references = settings_references(&repo, ADMIN_NEW);
        assert_eq!(references.is_empty(), excluded, "{ipc_new}\n{references:?}");
    }
}

/// 変更元か、差分のほかの (本体だけを変えた 2 つの) ファイルのどれかを、変えていないファイルが
/// `#[path]` で別の module にも取り込むときは除外しない (取り込み元と別の `ui/src/panel.rs` の参照も残る)。
#[test]
fn impact_rust_private_type_keeps_references_when_a_changed_file_is_mounted_elsewhere() {
    let ipc_new = IPC_LIB.replacen("    true\n", "    !false\n", 1);
    let bar_new = BAR.replace(
        "settings.visible.then_some(Choice::Settings)",
        "settings.visible.then(|| Choice::Settings)",
    );
    assert_ne!(bar_new, BAR);
    for (mount, excluded) in [
        ("", true),
        ("#[path = \"../../ipc/src/lib.rs\"]\nmod ipc_copy;\n", false),
        ("#[path = \"bar.rs\"]\nmod bar_copy;\n", false),
        (
            "#[path = \"../../engine/src/server/admin.rs\"]\nmod admin_copy;\n",
            false,
        ),
    ] {
        let repo = workspace(ADMIN_OLD, BAR);
        if !mount.is_empty() {
            repo.write("crates/tip/src/lib.rs", format!("{mount}{TIP_LIB}"));
            repo.commit_all("mounted copy");
        }
        repo.write("crates/ipc/src/lib.rs", &ipc_new);
        repo.write("crates/tip/src/bar.rs", &bar_new);
        let references = settings_references(&repo, ADMIN_NEW);
        if excluded {
            assert!(references.is_empty(), "{references:?}");
        } else {
            assert!(
                references
                    .iter()
                    .any(|line| line.starts_with("crates/ui/src/panel.rs:")),
                "{mount}\n{references:?}"
            );
        }
    }
}

/// `#[path]` で読んだファイルは mod.rs と同じく子の `mod` を自分のディレクトリから探すので、変更元の
/// 子のモジュールが別のクレートの `bridge.rs` を取り込むと、その `mod calls;` が読む隣の `calls.rs`
/// (別のクレートでは通常の `ui::calls`) も変更元のモジュールの子になる。その参照は除外しない。
#[test]
fn impact_rust_private_type_keeps_files_nested_under_a_path_mount() {
    for (mount, excluded) in [
        ("", true),
        (
            "#[path = \"../../../../ui/src/bridge.rs\"]\nmod bridge;\n",
            false,
        ),
    ] {
        let repo = workspace(&format!("mod child;\n{ADMIN_OLD}"), BAR);
        repo.create_dir_all("crates/engine/src/server/admin");
        repo.write(
            "crates/engine/src/server/admin/child.rs",
            format!("use super::*;\n{mount}"),
        );
        repo.write("crates/ui/src/lib.rs", format!("mod calls;\n{UI_LIB}"));
        repo.write("crates/ui/src/bridge.rs", "use super::*;\nmod calls;\n");
        repo.write(
            "crates/ui/src/calls.rs",
            "use super::*;\nfn take(_: &Settings) {}\n",
        );
        repo.commit_all("bridge");
        let references = settings_references(&repo, &format!("mod child;\n{ADMIN_NEW}"));
        let nested = "crates/ui/src/calls.rs: fn take(_: &Settings) {}";
        assert_eq!(
            !references.contains(nested),
            excluded,
            "{mount}\n{references:?}"
        );
    }
}

/// 変更元の `#[derive(Debug)]` は、どのソースも `Debug` を差し替えていないときだけ標準の derive と
/// みなし、非公開型の除外を保つ。
#[test]
fn impact_rust_private_type_needs_an_unshadowed_derive() {
    let derive = |admin: &str| {
        admin.replace(
            "pub(super) struct Administration",
            "#[derive(Debug)]\npub(super) struct Administration",
        )
    };
    // 関数の外のマクロの呼び出しは、derive の名前の差し替えを確かめられなくする。
    let bar = BAR.replace("macro_rules! noop {\n    () => {};\n}\nnoop!();\n", "");
    let repo = workspace(&derive(ADMIN_OLD), &bar);
    let references = settings_references(&repo, &derive(ADMIN_NEW));
    assert!(references.is_empty(), "{references:?}");
    for (path, source) in [
        (
            "crates/ui/src/panel.rs",
            format!("use injector::Custom as Debug;\n{PANEL}"),
        ),
        ("crates/tip/src/bar.rs", BAR.to_owned()),
    ] {
        let repo = workspace(&derive(ADMIN_OLD), &bar);
        repo.write(path, source);
        repo.commit_all("unproven derive");
        let references = settings_references(&repo, &derive(ADMIN_NEW));
        assert!(!references.is_empty(), "{path}");
    }
}

#[test]
fn impact_rust_private_type_keeps_a_glob_imported_variant() {
    let choice = "pub(super) enum Choice {\n    Settings,\n    Direct,\n}\n";
    let repo = workspace(&format!("{choice}{ADMIN_OLD}"), BAR);
    repo.write("crates/engine/src/server.rs", "mod admin;\nmod view;\n");
    repo.write("crates/engine/src/server/view.rs", "use super::admin::Choice::{self, *};\nfn value(c: Choice) -> u8 {\n    match c {\n        Settings => 1,\n        Direct => 0,\n    }\n}\n");
    repo.commit_all("glob imported variant");
    for variant in ["Settings { x: u8 }", "Renamed"] {
        let new = format!(
            "{}{ADMIN_NEW}",
            choice.replace("Settings,", &format!("{variant},"))
        );
        let references = settings_references(&repo, &new);
        assert!(
            references.contains("crates/engine/src/server/view.rs: Settings => 1,"),
            "{references:?}"
        );
    }
}
