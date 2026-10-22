//! Rust の自由関数と関連メンバーの位置を区別する API 判定。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

fn api_after(files: &[(&str, &str)], changes: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut all = vec![
        (
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", "pub mod api;\npub mod client;\n"),
    ];
    all.extend_from_slice(files);
    git_commit_files(repo, &all, "base");
    for (path, source) in changes {
        fs::write(repo.join(path), source).unwrap();
    }
    detect_api_changes_from_worktree(repo)
}

#[test]
fn rust_reference_sites_updated_free_function_ignores_associated_members() {
    let old = "pub fn put(count: u32) -> u32 {\n count * 2\n}\n";
    let new = "pub fn put(count: u32, label: &str) -> u32 {\n count * 2 + label.len() as u32\n}\n";
    let client = "use crate::api;\npub struct Client;\nimpl Client {\n pub fn put(&self, count: u32) -> u32 {\n  api::put(count)\n }\n}\npub fn handle(client: &Client) -> u32 { client.put(3) }\n";
    let updated = client.replace("api::put(count)", "api::put(count, \"a\")");
    let api = api_after(
        &[("src/api.rs", old), ("src/client.rs", client)],
        &[("src/api.rs", new), ("src/client.rs", &updated)],
    );
    assert!(api.modified.iter().all(|s| s.name != "put"), "{api:?}");
    assert!(
        api.modified_closed_in_diff.iter().any(|s| s.name == "put"),
        "{api:?}"
    );
}

#[test]
fn rust_reference_sites_removed_free_function_ignores_associated_members() {
    let client = "pub struct Engine;\nimpl Engine {\n pub fn run(&self) -> u32 { 1 }\n}\npub fn drive(engine: &Engine) -> u32 { engine.run() }\n";
    let api = api_after(
        &[
            ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
            ("src/client.rs", client),
        ],
        &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
    );
    assert!(api.removed.iter().all(|s| s.name != "run"), "{api:?}");
    assert!(api.removed_dead.iter().any(|s| s.name == "run"), "{api:?}");
}

#[test]
fn rust_reference_sites_target_inner_attributes_disable_free_function_proofs() {
    let old = "pub fn put(n: u32) -> u32 { #![rewrite] n }\n";
    let new = "pub fn put(n: u32, extra: u32) -> u32 { #![rewrite] n + extra }\n";
    let client = "pub struct Client; impl Client { pub fn put(&self) {} } pub fn call(c: &Client) { c.put(); crate::api::put(1); }\n";
    let updated = client.replace("crate::api::put(1)", "crate::api::put(1, 2)");
    let api = api_after(
        &[("src/api.rs", old), ("src/client.rs", client)],
        &[("src/api.rs", new), ("src/client.rs", &updated)],
    );
    assert!(api.modified.iter().any(|s| s.name == "put"), "{api:?}");
    assert!(
        api.modified_closed_in_diff.iter().all(|s| s.name != "put"),
        "{api:?}"
    );
    let api = api_after(
        &[
            ("src/api.rs", old),
            (
                "src/client.rs",
                "pub struct Client; impl Client { pub fn put(&self) {} } pub fn call(c: &Client) { c.put(); }\n",
            ),
        ],
        &[("src/api.rs", "pub fn launch() -> u32 { 1 }\n")],
    );
    assert!(api.removed.iter().any(|s| s.name == "put"), "{api:?}");
    assert!(api.removed_dead.iter().all(|s| s.name != "put"), "{api:?}");
}

#[test]
fn rust_reference_sites_closure_preserves_unupdated_and_ambiguous_uses() {
    let old = "pub fn put(n: u32) -> u32 {\n n\n}\n";
    let new = "pub fn put(n: u32, extra: u32) -> u32 {\n n + extra\n}\n";
    let caller_old = "pub fn call() -> u32 { crate::api::put(1) }\n";
    let caller_new = "pub fn call() -> u32 { crate::api::put(1, 2) }\n";
    for extra in [
        "pub struct Client; impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { c.put(1); crate::api::put(1); }\n",
        "use crate::api::put; pub fn call() { put(1); }\n",
        "use crate::api::put as save; pub fn call() { save(1); }\n",
        "pub fn call() { let f = crate::api::put; f(1); }\n",
        "pub fn put(n: u32) -> u32 { n }\n",
        "impl Client { fn outer() { fn put(n: u32) -> u32 { n } } }\n",
        "pub struct Client { pub put: fn(u32) -> u32 } impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { (c.put)(1); }\n",
        "pub struct Client { pub put: fn(u32) -> u32 } impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { let f = c.put; f(1); }\n",
        "macro_rules! m { ($x:expr) => { $x }; } pub fn call(c: &Client) { m!(c.put(1)); }\n",
        "#[rewrite] impl Client { pub fn put(&self, n: u32) -> u32 { n } }\n",
        "#![rewrite]\npub struct Client; impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { c.put(1); }\n",
        "pub fn malformed( { c.put(1); }\n",
    ] {
        let api = api_after(
            &[
                ("src/api.rs", old),
                ("src/client.rs", caller_old),
                ("extra.rs", extra),
            ],
            &[("src/api.rs", new), ("src/client.rs", caller_new)],
        );
        assert!(
            api.modified.iter().any(|s| s.name == "put"),
            "{extra}: {api:?}"
        );
        assert!(
            api.modified_closed_in_diff.iter().all(|s| s.name != "put"),
            "{extra}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_closure_handles_members_in_target_and_other_crates() {
    let methods = "pub struct Client;\nimpl Client { pub fn put(&self, n: u32) -> u32 { n } }\n";
    for method_path in ["src/api.rs", "src/client.rs", "other/src/lib.rs"] {
        let old = format!(
            "pub fn put(n: u32) -> u32 {{\n n\n}}\n{}",
            if method_path == "src/api.rs" {
                methods
            } else {
                ""
            }
        );
        let new = old.replace("put(n: u32)", "put(n: u32, extra: u32)");
        let client_old = format!(
            "{}pub fn call(c: &Client) -> u32 {{ c.put(crate::api::put(1)) }}\n",
            if method_path == "src/client.rs" {
                methods
            } else {
                ""
            }
        );
        let client_new = client_old.replace("crate::api::put(1)", "crate::api::put(1, 2)");
        let mut files = vec![
            ("src/api.rs", old.as_str()),
            ("src/client.rs", client_old.as_str()),
        ];
        if method_path == "other/src/lib.rs" {
            files.push((method_path, methods));
            files.push((
                "other/Cargo.toml",
                "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ));
        }
        let api = api_after(
            &files,
            &[("src/api.rs", &new), ("src/client.rs", &client_new)],
        );
        assert!(
            api.modified_closed_in_diff.iter().any(|s| s.name == "put"),
            "{method_path}: {api:?}"
        );
        assert!(
            api.modified.iter().all(|s| s.name != "put"),
            "{method_path}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_removal_preserves_function_values_and_real_uses() {
    let methods = "pub struct Engine { pub run: fn() -> u32 }\nimpl Engine { pub fn run(&self) -> u32 { 1 } }\n";
    for usage in [
        "pub fn drive() -> u32 { crate::api::run() }\n",
        "use crate::api::run; pub fn drive() -> u32 { run() }\n",
        "use crate::api::run as go; pub fn drive() -> u32 { go() }\n",
        "pub fn drive() { let f = crate::api::run; f(); }\n",
        "pub fn drive(e: &Engine) -> u32 { (e.run)() }\n",
        "pub fn drive(e: &Engine) { let f = e.run; f(); }\n",
        "pub fn drive(e: &Engine) { consume(e.run); }\n",
        "pub fn get(e: &Engine) -> fn() -> u32 { e.run }\n",
        "pub fn get(e: &Engine) -> (fn() -> u32,) { (e.run,) }\n",
        "pub fn get(e: &Engine) -> [fn() -> u32; 1] { [e.run] }\n",
        "#[rewrite] pub fn get(e: &Engine) -> fn() -> u32 { e.run }\n",
        "pub struct Count { pub run: u32 } pub fn get(e: &Engine) -> fn() -> u32 { e.run }\n",
        "pub struct Count { pub run: u32 } pub fn get(e: &Count) -> fn() -> u32 { let e = Engine { run: fallback }; e.run }\n",
        "pub fn drive() { Engine::run(); }\n",
        "pub fn drive(e: &Engine) { e.run::<u32>(); }\n",
        "pub fn drive(e: &Engine) { m!(e.run()); }\n",
        "#[rewrite] pub fn drive(e: &Engine) { e.run(); }\n",
        "pub mod nested { #![rewrite] pub fn drive(e: &super::Engine) { e.run(); } }\n",
        "pub fn drive(e: &Engine) { #![rewrite] e.run(); }\n",
        "pub fn malformed( { e.run(); }\n",
    ] {
        let client = format!("{methods}{usage}");
        let api = api_after(
            &[
                ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
                ("src/client.rs", &client),
            ],
            &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
        );
        assert!(
            api.removed.iter().any(|s| s.name == "run"),
            "{usage}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().all(|s| s.name != "run"),
            "{usage}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_removal_handles_multiple_members_and_keeps_method_removal() {
    let methods = "pub struct A; impl A { pub fn run(&self) -> u32 { 1 } }\npub struct B; impl B { pub fn run(&self) -> u32 { 2 } }\npub trait Runner { fn run(&self) -> u32; }\n";
    let api = api_after(
        &[
            ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
            ("src/client.rs", methods),
        ],
        &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
    );
    assert!(api.removed_dead.iter().any(|s| s.name == "run"), "{api:?}");
    let api = api_after(
        &[
            (
                "src/api.rs",
                "pub struct Old;\nimpl Old { pub fn run(&self) -> u32 { 7 } }\n",
            ),
            ("src/client.rs", methods),
        ],
        &[("src/api.rs", "pub struct Old;\n")],
    );
    assert!(api.removed.iter().any(|s| s.name == "Old.run"), "{api:?}");
    assert!(
        api.removed_dead.iter().all(|s| s.name != "Old.run"),
        "{api:?}"
    );
}

#[test]
fn rust_reference_sites_removal_distinguishes_other_crate_methods_and_free_calls() {
    for free_call in [false, true] {
        let consumer = format!(
            "pub struct Engine; impl Engine {{ pub fn run(&self) -> u32 {{ 1 }} }} pub fn drive(e: &Engine) -> u32 {{ e.run(){} }}\n",
            if free_call { " + demo::api::run()" } else { "" }
        );
        let api = api_after(
            &[
                ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
                ("src/client.rs", ""),
                ("other/src/lib.rs", &consumer),
                (
                    "other/Cargo.toml",
                    "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ndemo = { path = \"../..\" }\n",
                ),
            ],
            &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
        );
        assert_eq!(
            api.removed.iter().any(|s| s.name == "run"),
            free_call,
            "{api:?}"
        );
        assert_eq!(
            api.removed_dead.iter().any(|s| s.name == "run"),
            !free_call,
            "{api:?}"
        );
    }
}
