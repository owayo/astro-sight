//! Rust の関連メンバーが API 降格と通常の出現一覧へ与える影響。

use super::support::*;

#[test]
fn rust_reference_sites_hook_distinguishes_modified_function_and_methods() {
    for unupdated in [false, true] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod api; pub mod client;\npub fn execute(c: &client::Client) -> u32 { client::call(c) }\n");
        repo.write("src/api.rs", "pub fn put(n: u32) -> u32 {\n n\n}\n");
        let client = format!(
            "pub struct Client;\nimpl Client {{ pub fn put(&self, n: u32) -> u32 {{ n }} }}\npub fn call(c: &Client) -> u32 {{ c.put(crate::api::put(1)) }}\n{}",
            if unupdated {
                "pub fn old_call() -> u32 { crate::api::put(1) }\n"
            } else {
                ""
            }
        );
        repo.write("src/client.rs", &client);
        repo.init_git();
        repo.commit_all("base");
        repo.write(
            "src/api.rs",
            "pub fn put(n: u32, extra: u32) -> u32 {\n n + extra\n}\n",
        );
        repo.write(
            "src/client.rs",
            client.replacen(
                "c.put(crate::api::put(1))",
                "c.put(crate::api::put(1, 2))",
                1,
            ),
        );
        let output = cargo_bin()
            .args([
                "review",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(i32::from(unupdated)),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        let key = if unupdated { "mod" } else { "mod_closed" };
        assert!(
            hook["api"][key]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["n"] == "put"),
            "{hook}"
        );
    }
}

#[test]
fn rust_reference_sites_hook_removal_keeps_occurrence_and_dead_code_contracts() {
    for real_call in [false, true] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod api; pub mod client;\n");
        repo.write("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n");
        repo.write("src/client.rs", format!("pub struct Engine;\nimpl Engine {{ pub fn run(&self) -> u32 {{ 1 }} }}\npub fn drive(e: &Engine) -> u32 {{ e.run(){} }}\n", if real_call { " + crate::api::run()" } else { "" }));
        repo.init_git();
        repo.commit_all("base");
        repo.write("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n");
        let output = cargo_bin()
            .args([
                "review",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(i32::from(real_call)),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        let key = if real_call { "rm" } else { "rm_dead" };
        assert!(
            hook["api"][key]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["n"] == "run"),
            "{hook}"
        );
        let refs = repo.run_json("refs", &["--name", "run"]);
        assert_eq!(
            refs["refs"].as_array().unwrap().len(),
            if real_call { 3 } else { 2 },
            "{refs}"
        );
        let dead = repo.run_json("dead-code", &[]);
        assert!(
            dead["dead_symbols"]
                .as_array()
                .is_none_or(|symbols| symbols.iter().all(|symbol| symbol["name"] != "Engine.run")),
            "{dead}"
        );
    }
}

/// `src/launcher.rs` の自由関数 `run` をファイルごと消し、`lib.rs` から `pub mod launcher;` を
/// 消した差分の hook 出力と終了コード。`files` は変えないファイル。
fn hook_after_removing_launcher(
    modules: &str,
    files: &[(&str, &str)],
) -> (TestRepo, Option<i32>, serde_json::Value) {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", format!("pub mod launcher;\n{modules}"));
    repo.write("src/launcher.rs", "pub fn run() -> u32 {\n    7\n}\n");
    for (path, source) in files {
        if let Some(parent) = std::path::Path::new(path).parent() {
            repo.create_dir_all(parent);
        }
        repo.write(path, source);
    }
    repo.init_git();
    repo.commit_all("base");
    repo.remove_file("src/launcher.rs");
    repo.write("src/lib.rs", modules);
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    let hook = serde_json::from_slice(&output.stderr).unwrap();
    (repo, output.status.code(), hook)
}

fn hook_lists(hook: &serde_json::Value, key: &str, name: &str) -> bool {
    hook["api"][key]
        .as_array()
        .is_some_and(|symbols| symbols.iter().any(|symbol| symbol["n"] == name))
}

/// Issue #110: 別モジュールの同名の自由関数や別の型の同名の関連関数をパス付きで呼ぶだけなら、
/// 呼ばれていない自由関数を消す差分は rm_dead にする。消したモジュールを指すパスや、消した関数の
/// import の後の素の呼び出しは api.rm に残す。refs の出現一覧は変えない。
#[test]
fn rust_reference_sites_hook_removal_resolves_qualified_paths() {
    let engine = "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    run()\n}\n";
    let modules = "pub mod engine;\npub mod app;\n";
    let (repo, code, hook) = hook_after_removing_launcher(
        modules,
        &[
            ("src/engine.rs", engine),
            (
                "src/app.rs",
                "pub fn start() -> u32 {\n    crate::engine::run()\n}\n",
            ),
        ],
    );
    assert_eq!(code, Some(0), "{hook}");
    assert!(hook_lists(&hook, "rm_dead", "run"), "{hook}");
    assert!(!hook_lists(&hook, "rm", "run"), "{hook}");
    let refs = repo.run_json("refs", &["--name", "run"]);
    assert_eq!(refs["refs"].as_array().unwrap().len(), 3, "{refs}");

    let (_, code, hook) = hook_after_removing_launcher(
        "pub mod engine;\n",
        &[(
            "src/engine.rs",
            "pub struct Engine;\n\nimpl Engine {\n    pub fn run() -> u32 {\n        1\n    }\n}\n\npub fn drive() -> u32 {\n    Engine::run()\n}\n",
        )],
    );
    assert_eq!(code, Some(0), "{hook}");
    assert!(hook_lists(&hook, "rm_dead", "run"), "{hook}");

    for app in [
        "pub fn start() -> u32 {\n    crate::launcher::run()\n}\n",
        "use crate::launcher::run;\n\npub fn start() -> u32 {\n    run()\n}\n",
    ] {
        let (_, code, hook) = hook_after_removing_launcher(
            modules,
            &[("src/engine.rs", engine), ("src/app.rs", app)],
        );
        assert_eq!(code, Some(1), "{app}: {hook}");
        assert!(hook_lists(&hook, "rm", "run"), "{app}: {hook}");
        assert!(!hook_lists(&hook, "rm_dead", "run"), "{app}: {hook}");
    }
}

#[test]
fn rust_reference_sites_hook_removal_audits_ignored_test_roots() {
    let modules = "pub mod engine; pub mod app;";
    let engine = "pub fn run() -> u32 { 1 }";
    let app = "pub fn start() -> u32 { super::engine::run() }";
    for extra in [
        vec![(
            "tests/local.rs",
            "use tool::launcher as engine; #[path = \"../src/app.rs\"] mod app; #[test] fn smoke() { app::start(); }",
        )],
        vec![(
            "examples/probe.rs",
            "macro_rules! mount { ($a:ident, $p:literal) => { #[$a = $p] mod app; }; } use tool::launcher as engine; mount!(path, \"../src/app.rs\"); fn main() { app::start(); }",
        )],
    ] {
        let mut files = vec![
            ("src/engine.rs", engine),
            ("src/app.rs", app),
            (".gitignore", "tests/\nexamples/\n"),
        ];
        files.extend(extra);
        let (_, code, hook) = hook_after_removing_launcher(modules, &files);
        assert_eq!(code, Some(1), "{files:?}: {hook}");
        assert!(hook_lists(&hook, "rm", "run"), "{hook}");
        assert!(!hook_lists(&hook, "rm_dead", "run"), "{hook}");
    }
    for extra in [
        vec![("build.rs", "fn main() {}")],
        vec![("src/main.rs", "mod engine; mod app; fn main() {}")],
    ] {
        let mut files = vec![("src/engine.rs", engine), ("src/app.rs", app)];
        files.extend(extra);
        let (_, code, hook) = hook_after_removing_launcher(modules, &files);
        assert_eq!(code, Some(0), "{files:?}: {hook}");
        assert!(hook_lists(&hook, "rm_dead", "run"), "{hook}");
    }
}

#[test]
fn rust_reference_sites_hook_removal_keeps_unproven_field_values_blocking() {
    for result in [
        "fn() -> u32 { e.run }",
        "(fn() -> u32,) { (e.run,) }",
        "[fn() -> u32; 1] { [e.run] }",
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod api; pub mod client;\n");
        repo.write("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n");
        repo.write("src/client.rs", format!("pub struct Engine {{ pub run: fn() -> u32 }}\nimpl Engine {{ pub fn run(&self) -> u32 {{ 1 }} }}\npub fn get(e: &Engine) -> {result}\n"));
        repo.init_git();
        repo.commit_all("base");
        repo.write("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n");
        let output = cargo_bin()
            .args([
                "review",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{result}");
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(
            hook["api"]["rm"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["n"] == "run"),
            "{result}: {hook}"
        );
        assert!(
            hook["api"]["rm_dead"]
                .as_array()
                .is_none_or(|symbols| symbols.iter().all(|symbol| symbol["n"] != "run")),
            "{result}: {hook}"
        );
    }
}
