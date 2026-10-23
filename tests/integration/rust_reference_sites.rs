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

/// workspace のクレート alpha の `items::has_flag` を beta へ移し、同じ名前・同じシグネチャの
/// 関数 `gamma_function` を gamma にも足した差分の hook 出力と終了コード (Issue #115)。
fn hook_after_moving_to_other_crate(
    alpha_lib: &str,
    gamma_function: &str,
) -> (Option<i32>, serde_json::Value) {
    hook_after_moving_to_other_crate_with_env(alpha_lib, gamma_function, None)
}

fn hook_after_moving_to_other_crate_with_env(
    alpha_lib: &str,
    gamma_function: &str,
    environment: Option<(&str, &str)>,
) -> (Option<i32>, serde_json::Value) {
    let has_flag = "pub fn has_flag(s: Option<&str>) -> bool {\n    s == Some(\"tasks\")\n}\n";
    let manifest = |name: &str, dependencies: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}"
        )
    };
    let repo = TestRepo::new();
    for directory in ["crates/alpha/src", "crates/beta/src", "crates/gamma/src"] {
        repo.create_dir_all(directory);
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/gamma\"]\nresolver = \"2\"\n",
    );
    repo.write("crates/alpha/Cargo.toml", manifest("alpha", ""));
    repo.write(
        "crates/alpha/src/lib.rs",
        "pub mod items;\n\npub fn check() -> bool {\n    items::has_flag(None)\n}\n",
    );
    repo.write("crates/alpha/src/items.rs", has_flag);
    repo.write("crates/beta/Cargo.toml", manifest("beta", ""));
    repo.write("crates/beta/src/lib.rs", "pub mod items;\n");
    repo.write("crates/beta/src/items.rs", "");
    repo.write("crates/gamma/Cargo.toml", manifest("gamma", ""));
    repo.write(
        "crates/gamma/src/lib.rs",
        "pub fn other() -> i32 {\n    1\n}\n",
    );
    repo.init_git();
    repo.commit_all("base");
    repo.remove_file("crates/alpha/src/items.rs");
    repo.write(
        "crates/alpha/Cargo.toml",
        manifest("alpha", "beta = { path = \"../beta\" }\n"),
    );
    repo.write("crates/beta/src/items.rs", has_flag);
    repo.write("crates/alpha/src/lib.rs", alpha_lib);
    repo.write(
        "crates/gamma/src/lib.rs",
        format!(
            "pub fn other() -> i32 {{\n    1\n}}\n\npub fn {gamma_function}(s: Option<&str>) -> bool {{\n    s == Some(\"tasks\")\n}}\n"
        ),
    );
    let mut command = cargo_bin();
    command.args([
        "review",
        "--dir",
        repo.root().to_str().unwrap(),
        "--git",
        "--hook",
    ]);
    if let Some((name, value)) = environment {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    let hook = serde_json::from_slice(&output.stderr).unwrap();
    (output.status.code(), hook)
}

/// Issue #115: 移動先が 2 つで moved にならない削除でも、依存クレートの名前で始まるパスが依存先の
/// 残る関数に解決できれば rm_dead にする。移動先が 1 つなら従来どおり moved、消した関数を指す
/// パスが残れば api.rm。
#[test]
fn rust_reference_sites_hook_removal_resolves_dependency_crate_paths() {
    let through_beta = "pub fn check() -> bool {\n    beta::items::has_flag(None)\n}\n";
    let (code, hook) = hook_after_moving_to_other_crate(through_beta, "has_flag");
    assert_eq!(code, Some(0), "{hook}");
    assert!(hook_lists(&hook, "rm_dead", "has_flag"), "{hook}");
    assert!(!hook_lists(&hook, "rm", "has_flag"), "{hook}");

    let (code, hook) = hook_after_moving_to_other_crate(through_beta, "has_other_flag");
    assert_eq!(code, Some(0), "{hook}");
    assert!(
        hook["api"]["moved"]
            .as_array()
            .is_some_and(|moved| moved.iter().any(|symbol| symbol["n"] == "has_flag")),
        "{hook}"
    );

    let (code, hook) = hook_after_moving_to_other_crate(
        "pub fn check() -> bool {\n    crate::items::has_flag(None)\n}\n",
        "has_flag",
    );
    assert_eq!(code, Some(1), "{hook}");
    assert!(hook_lists(&hook, "rm", "has_flag"), "{hook}");
}

#[test]
fn rust_reference_sites_dependency_paths_keep_compiler_overrides_blocking() {
    let through_beta = "pub fn check() -> bool { beta::items::has_flag(None) }\n";
    for environment in [
        ("RUSTFLAGS", "--extern beta=other.rlib"),
        ("CARGO_ENCODED_RUSTFLAGS", "--extern\u{1f}beta=other.rlib"),
        ("RUSTC_WRAPPER", "unknown-compiler-wrapper"),
    ] {
        let (code, hook) =
            hook_after_moving_to_other_crate_with_env(through_beta, "has_flag", Some(environment));
        assert_eq!(code, Some(1), "{environment:?}: {hook}");
        assert!(hook_lists(&hook, "rm", "has_flag"), "{hook}");
    }
}
