//! Rust の inherent メソッドの変更で、別の型のパスの呼び出しを呼び出し元にしない (Issue #470)。

use std::collections::BTreeSet;

use super::support::*;

const LIB: &str = "pub mod config;\npub mod server;\npub struct Engine;\nimpl Engine {\n    pub fn new() -> Self {\n        Engine\n    }\n    pub fn handle(&self, id: u32) -> u32 {\n        id\n    }\n}\n";
const CONFIG: &str = "#[derive(Clone, Default)]\npub struct Config {\n    pub learning: bool,\n}\npub fn new() -> Config {\n    Config::default()\n}\n";
/// Issue の最小の再現の `server.rs`。`Box::new(config::new())` (prelude の型とモジュールの
/// 自由関数) を足してある。
const SERVER: &str = "use crate::{Engine, config};\nmod admin;\nuse std::sync::{Arc, Mutex};\npub fn serve() -> std::io::Result<String> {\n    let engine = Engine::new();\n    let _ = engine.handle(1);\n    let admin = Arc::new(admin::Administration::new(config::Config::default()));\n    let shared = Arc::new(Mutex::new(0));\n    let boxed = Box::new(config::new());\n    let worker = std::thread::Builder::new().spawn(move || *shared.lock().unwrap());\n    let _ = (worker, boxed);\n    admin.handle()\n}\n";
const ADMIN_OLD: &str = "use super::*;\npub(super) struct Administration {\n    settings: Mutex<config::Config>,\n}\nimpl Administration {\n    pub(super) fn new(settings: config::Config) -> Self {\n        Self {\n            settings: Mutex::new(settings),\n        }\n    }\n    pub(super) fn handle(&self) -> std::io::Result<String> {\n        let current = self.settings.lock().unwrap();\n        Ok(format!(\"{}\", current.learning && ipc::ping()))\n    }\n}\n";
const ADMIN_NEW: &str = "use super::*;\npub(super) struct Administration {\n    settings: Mutex<Option<config::Config>>,\n}\nimpl Administration {\n    pub(super) fn new(settings: Option<config::Config>) -> Self {\n        Self {\n            settings: Mutex::new(settings),\n        }\n    }\n    pub(super) fn handle(&self) -> std::io::Result<String> {\n        let current = self.settings.lock().unwrap();\n        Ok(format!(\"{}\", current.is_some() && ipc::ping()))\n    }\n}\n";

/// 真の呼び出し元の行。
const ADMIN_CALL: &str =
    "let admin = Arc::new(admin::Administration::new(config::Config::default()));";
/// 別の型の `new` を呼ぶ行 (crate の型、`use` した std の型、prelude の型とモジュールの自由関数、
/// std のフルパス)。
const OTHER_CALLS: [&str; 4] = [
    "let engine = Engine::new();",
    "let shared = Arc::new(Mutex::new(0));",
    "let boxed = Box::new(config::new());",
    "let worker = std::thread::Builder::new().spawn(move || *shared.lock().unwrap());",
];

/// ipc・engine・tip の 3 つのクレートのワークスペース。engine は ipc に依存する。
fn workspace(server: &str, admin: &str) -> TestRepo {
    let repo = TestRepo::new();
    for package in ["ipc", "engine", "tip"] {
        repo.create_dir_all(format!("{package}/src"));
        let dependencies = if package == "ipc" {
            ""
        } else {
            "\n[dependencies]\nipc = { path = \"../ipc\" }\n"
        };
        repo.write(
            format!("{package}/Cargo.toml"),
            format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{dependencies}"),
        );
    }
    repo.create_dir_all("engine/src/server");
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"ipc\", \"engine\", \"tip\"]\nresolver = \"2\"\n",
    );
    repo.write("ipc/src/lib.rs", "pub fn ping() -> bool {\n    true\n}\n");
    repo.write(
        "tip/src/lib.rs",
        "pub fn show() -> bool {\n    ipc::ping()\n}\n",
    );
    repo.write("engine/src/lib.rs", LIB);
    repo.write("engine/src/config.rs", CONFIG);
    repo.write("engine/src/server.rs", server);
    repo.write("engine/src/server/admin.rs", admin);
    repo.init_git();
    repo.commit_all("initial");
    repo
}

/// `engine/src/server/admin.rs` の変更の、`symbol` の呼び出し元の行の本文 (前後の空白を除く)。
/// (blocking の一覧, low・informational を含む全部の一覧) を返す。
fn caller_lines(
    repo: &TestRepo,
    symbol: &str,
    args: &[&str],
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut arguments = vec!["--git"];
    arguments.extend_from_slice(args);
    let context = repo.run_json("context", &arguments);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "engine/src/server/admin.rs")
        .unwrap_or_else(|| panic!("admin.rs is not analyzed: {context}"));
    let lines = |key: &str| -> BTreeSet<String> {
        change[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|caller| {
                caller["path"] != "engine/src/server/admin.rs"
                    && caller["symbols"]
                        .as_array()
                        .is_some_and(|symbols| symbols.iter().any(|name| name == symbol))
            })
            .map(|caller| repo.caller_source_line(caller))
            .collect()
    };
    let blocking = lines("impacted_callers");
    let mut all = blocking.clone();
    all.extend(lines("low_confidence_callers"));
    all.extend(lines("informational_callers"));
    (blocking, all)
}

/// `server.rs` の末尾に `extra` を足し、`Administration::new` の引数を変えたときの `new` の呼び出し元。
fn new_callers(extra: &str, args: &[&str]) -> (BTreeSet<String>, BTreeSet<String>) {
    let repo = workspace(&format!("{SERVER}{extra}"), ADMIN_OLD);
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    if args.contains(&"--staged") {
        repo.stage_all();
    }
    caller_lines(&repo, "new", args)
}

#[test]
fn impact_rust_owner_paths_drop_calls_on_other_types() {
    // 所有型と違う名前の型・モジュールのパスの `new` は、low・informational にも出さない。
    let (blocking, all) = new_callers("", &[]);
    assert_eq!(blocking, BTreeSet::from([ADMIN_CALL.to_owned()]));
    assert_eq!(all, blocking);
}

#[test]
fn impact_rust_owner_paths_keep_unresolved_bindings() {
    for (extra, expected) in [
        (
            "use admin::Administration as Other;\nfn retry() {\n    let _ = Other::new(None);\n}\n",
            "let _ = Other::new(None);",
        ),
        (
            "type Other = admin::Administration;\nfn retry() {\n    let _ = Other::new(None);\n}\n",
            "let _ = Other::new(None);",
        ),
        (
            "impl admin::Administration {\n    fn retry() -> Self {\n        Self::new(None)\n    }\n}\n",
            "Self::new(None)",
        ),
        (
            "fn retry() {\n    use admin::Administration as Engine;\n    let _ = Engine::new(None);\n}\n",
            "let _ = Engine::new(None);",
        ),
        (
            "fn retry() {\n    type Box = admin::Administration;\n    let _ = Box::new(None);\n}\n",
            "let _ = Box::new(None);",
        ),
    ] {
        let (blocking, _) = new_callers(extra, &[]);
        assert!(blocking.contains(expected), "{blocking:?}");
        assert!(blocking.contains(ADMIN_CALL), "{blocking:?}");
    }
    let (blocking, _) = new_callers("", &["--staged"]);
    assert!(blocking.contains(OTHER_CALLS[0]), "{blocking:?}");
}

#[test]
fn impact_rust_owner_paths_keep_trait_and_aliased_impl_targets() {
    for admin in [
        "pub(super) struct Administration;\npub(super) trait Build { fn new(x: u8) -> Self; }\nimpl Build for Administration {\n    fn new(x: u8) -> Self { Self }\n}\n",
        "use super::*;\ntype Administration = Engine;\nimpl Administration {\n    pub(super) fn new(x: u8) -> Self { Engine }\n}\n",
    ] {
        let repo = workspace(SERVER, admin);
        repo.write(
            "engine/src/server/admin.rs",
            admin.replace("x: u8", "x: u16"),
        );
        let (blocking, _) = caller_lines(&repo, "new", &[]);
        assert!(blocking.contains(OTHER_CALLS[0]), "{blocking:?}");
    }
}

const RECEIVERS: &str = r#"pub fn dispatch(engine: &mut Engine, manage: u32, tx: u32) -> std::io::Result<()> {
    let admin = admin::Administration::new(config::Config::default());
    engine.handle(manage);
    admin.handle(manage, tx)?;
    Ok(())
}
"#;

#[test]
fn impact_rust_owner_paths_keep_only_the_changed_constructor_and_receiver() {
    let old = ADMIN_OLD.replace("fn handle(&self)", "fn handle(&self, manage: u32, tx: u32)");
    let new = ADMIN_NEW.replace("fn handle(&self)", "fn handle(&self, manage: u32, tx: u64)");
    let server =
        format!("use crate::{{Engine, config}};\nmod admin;\nuse std::sync::Mutex;\n{RECEIVERS}");
    let repo = workspace(&server, &old);
    repo.write("engine/src/server/admin.rs", new);
    let output = cargo_bin()
        .current_dir(repo.root())
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected: Vec<_> = server
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            line.contains("admin::Administration::new(")
                || line.contains("admin.handle(manage, tx)?")
        })
        .map(|(index, line)| {
            format!(
                "→ engine/src/server.rs:{} [{}]",
                index + 1,
                if line.contains("::new(") {
                    "new"
                } else {
                    "handle"
                }
            )
        })
        .collect();
    let actual: Vec<_> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('→'))
        .collect();
    assert_eq!(actual, expected, "{stderr}");
}

#[test]
fn impact_rust_owner_paths_keep_shadowed_receivers_and_deref_targets() {
    let extra = r#"
fn shadow(engine: &mut Engine, admin: &admin::Administration) {
    if true {
        let engine = admin;
        engine.handle();
    }
}
fn forwarded(wrapper: &Wrapper) {
    wrapper.handle();
}
"#;
    let old = ADMIN_OLD.replace(
        "pub(super) struct Administration",
        "pub struct Administration",
    );
    let new = ADMIN_NEW.replace(
        "pub(super) struct Administration",
        "pub struct Administration",
    );
    let repo = workspace(
        &format!("use crate::Wrapper;\n{SERVER}pub use admin::Administration as Admin;\n{extra}"),
        &old,
    );
    repo.write("engine/src/lib.rs", format!("{LIB}pub struct Wrapper(server::Admin);\nimpl std::ops::Deref for Wrapper {{\n    type Target = server::Admin;\n    fn deref(&self) -> &Self::Target {{ &self.0 }}\n}}\n"));
    repo.commit_all("wrapper");
    repo.write(
        "engine/src/server/admin.rs",
        new.replace("fn handle(&self)", "fn handle(&self, detail: u8)"),
    );
    let (blocking, _) = caller_lines(&repo, "handle", &[]);
    for expected in ["engine.handle();", "wrapper.handle();", "admin.handle()"] {
        assert!(blocking.contains(expected), "{blocking:?}");
    }
}

#[test]
fn impact_rust_owner_paths_require_generic_bounds_before_excluding_deref() {
    let library = r#"pub mod config;
pub mod server;
use ipc::Backend;
pub struct Engine<B>(pub B, pub server::Admin);
impl<B: Backend> Engine<B> {
    pub fn handle(&self) {}
}
impl<B> std::ops::Deref for Engine<B> {
    type Target = server::Admin;
    fn deref(&self) -> &Self::Target { &self.1 }
}
"#;
    let server = r#"use crate::{Engine, config};
use ipc::{Active, Inactive};
use std::sync::Mutex;
mod admin;
pub use admin::Administration as Admin;
fn first(active: &Engine<Active>) {
    active.handle();
}
fn second(inactive: &Engine<Inactive>) {
    inactive.handle();
}
"#;
    let old = ADMIN_OLD.replace(
        "pub(super) struct Administration",
        "pub struct Administration",
    );
    let repo = workspace(server, &old);
    repo.write("ipc/src/lib.rs", "pub trait Backend {}\npub struct Active;\npub struct Inactive;\nimpl Backend for Active {}\npub fn ping() -> bool { true }\n");
    repo.write("engine/src/lib.rs", library);
    repo.commit_all("generic receivers");
    repo.write(
        "engine/src/server/admin.rs",
        old.replace("fn handle(&self)", "fn handle(&self, detail: u8)"),
    );
    let (blocking, all) = caller_lines(&repo, "handle", &[]);
    assert!(blocking.contains("inactive.handle();"), "{blocking:?}");
    assert!(!all.contains("active.handle();"), "{all:?}");
}

#[test]
fn impact_rust_owner_paths_keep_calls_inside_an_attributed_method() {
    let server = format!(
        "{SERVER}\npub struct Server;\nimpl Server {{\n    #[injector::with_prelude]\n    fn retry() {{\n        let local = Engine::new();\n        let shared_retry = Arc::new(Mutex::new(0));\n    }}\n}}\n"
    );
    let repo = workspace(&server, ADMIN_OLD);
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    let (blocking, all) = caller_lines(&repo, "new", &[]);
    for line in [
        ADMIN_CALL,
        "let local = Engine::new();",
        "let shared_retry = Arc::new(Mutex::new(0));",
    ] {
        assert!(blocking.contains(line), "{blocking:?}");
    }
    assert!(!all.contains(OTHER_CALLS[0]), "{all:?}");
}

#[test]
fn impact_rust_owner_paths_allow_an_unrelated_body_change() {
    let repo = workspace(SERVER, ADMIN_OLD);
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    repo.write(
        "tip/src/lib.rs",
        "pub fn show() -> bool { ipc::ping() && true }\n",
    );
    assert_eq!(
        caller_lines(&repo, "new", &[]).1,
        BTreeSet::from([ADMIN_CALL.to_owned()])
    );
    // 旧側で別のファイルをこのモジュールに取り込んでいた場合は配置が変わる。
    repo.write("engine/src/server/admin.rs", ADMIN_OLD);
    repo.write(
        "tip/src/lib.rs",
        "include!(\"../../engine/src/server.rs\");\n",
    );
    repo.commit_all("alternate placement");
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    repo.write("tip/src/lib.rs", "pub fn show() -> bool { ipc::ping() }\n");
    assert!(caller_lines(&repo, "new", &[]).0.contains(OTHER_CALLS[0]));
}

/// `#[derive(Debug)]` が作る `Debug::fmt` も `fmt` の宣言 (inherent メソッドでない宣言) なので、
/// 変更した inherent の `fmt` の所有型で、ほかのパスの `fmt` の呼び出しを除かない。
#[test]
fn impact_rust_owner_paths_keep_trait_calls_of_a_derived_method_name() {
    let old = "use super::*;\n#[derive(Debug)]\npub(super) struct Administration;\nimpl Administration {\n    pub(super) fn fmt(&self) -> String {\n        String::new()\n    }\n}\n";
    let new = old.replace(
        "fn fmt(&self) -> String {\n        String::new()",
        "fn fmt(&self, verbose: bool) -> String {\n        verbose.to_string()",
    );
    // `Debug` の名前を import すると derive の差し替えを確かめられないので、module を別名で使う。
    let server = "use crate::config;\nmod admin;\nuse std::fmt as formatting;\nuse std::sync::Mutex;\npub fn show(admin: &admin::Administration, f: &mut formatting::Formatter<'_>) -> formatting::Result {\n    formatting::Debug::fmt(admin, f)\n}\n";
    let repo = workspace(server, old);
    repo.write("engine/src/server/admin.rs", new);
    let (_, all) = caller_lines(&repo, "fmt", &[]);
    assert!(all.contains("formatting::Debug::fmt(admin, f)"), "{all:?}");
}

#[test]
fn impact_rust_owner_paths_allow_unshadowed_builtin_derive() {
    let derive =
        |admin: &str| admin.replace("pub(super) struct", "#[derive(Debug)]\npub(super) struct");
    let repo = workspace(SERVER, &derive(ADMIN_OLD));
    repo.write("engine/src/server/admin.rs", derive(ADMIN_NEW));
    assert_eq!(
        caller_lines(&repo, "new", &[]).1,
        BTreeSet::from([ADMIN_CALL.to_owned()])
    );
    let repo = workspace(
        &format!("use injector::Custom as Debug;\n{SERVER}"),
        &derive(ADMIN_OLD),
    );
    repo.write("engine/src/server/admin.rs", derive(ADMIN_NEW));
    assert!(caller_lines(&repo, "new", &[]).0.contains(OTHER_CALLS[0]));
}
