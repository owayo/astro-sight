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
