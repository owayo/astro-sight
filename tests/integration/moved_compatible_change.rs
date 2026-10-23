//! Rust の互換な宣言変更と移動を、hook の出力と終了コードまで確かめる。

use super::support::*;

fn package(name: &str, dependencies: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}"
    )
}

fn contains(hook: &serde_json::Value, bucket: &str, name: &str) -> bool {
    hook["api"][bucket]
        .as_array()
        .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == name))
}

#[test]
fn moved_compatible_change_hook_accepts_updated_paths_and_blocks_stale_paths() {
    let expanded = "use std::collections::HashMap;\npub fn count(marks: &HashMap<String, String>) -> usize {\n    marks.len()\n}\n";
    let aliased = "use std::collections::HashMap;\npub type Tags = HashMap<String, String>;\npub fn count(marks: &Tags) -> usize {\n    marks.len()\n}\n";
    for (name, before, after, call, bucket) in [
        (
            "WEEK",
            "pub const WEEK: [(&str, u8); 2] = [(\"Mon\", 1), (\"Tue\", 2)];\n",
            "const NAMES: [&str; 2] = [\"Mon\", \"Tue\"];\npub const WEEK: [(&str, u8); 2] = [(NAMES[0], 1), (NAMES[1], 2)];\n",
            "WEEK.len()",
            "const_value",
        ),
        (
            "count",
            expanded,
            aliased,
            "count(&std::collections::HashMap::new())",
            "mod_compat",
        ),
        (
            "count",
            aliased,
            expanded,
            "count(&std::collections::HashMap::new())",
            "mod_compat",
        ),
        (
            "double",
            "pub fn double(x: i32) -> i32 { x * 2 }\n",
            "pub const fn double(x: i32) -> i32 { x * 2 }\n",
            "double(2)",
            "mod_compat",
        ),
    ] {
        for updated in [true, false] {
            let repo = TestRepo::new();
            for name in ["alpha", "beta", "consumer"] {
                repo.create_dir_all(format!("crates/{name}/src"));
                repo.write(format!("crates/{name}/Cargo.toml"), package(name, ""));
            }
            repo.write("Cargo.toml", "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/consumer\"]\nresolver = \"2\"\n");
            repo.write("crates/alpha/src/lib.rs", "pub mod items;\n");
            repo.write("crates/beta/src/lib.rs", "pub mod items;\n");
            repo.write("crates/alpha/src/items.rs", before);
            repo.write("crates/beta/src/items.rs", "");
            repo.write(
                "crates/consumer/Cargo.toml",
                package("consumer", "alpha = { path = \"../alpha\" }\n"),
            );
            let caller = |krate: &str, comment: &str| {
                format!(
                    "fn main() {{\n    println!(\"{{}}\", {krate}::items::{call}); {comment}\n}}\n"
                )
            };
            repo.write("crates/consumer/src/main.rs", caller("alpha", ""));
            repo.init_git();
            repo.commit_all("base");
            repo.git(["rm", "crates/alpha/src/items.rs"]);
            repo.write("crates/alpha/src/lib.rs", "");
            repo.write("crates/beta/src/items.rs", after);
            repo.write(
                "crates/consumer/Cargo.toml",
                package("consumer", "beta = { path = \"../beta\" }\n"),
            );
            repo.write(
                "crates/consumer/src/main.rs",
                caller(if updated { "beta" } else { "alpha" }, "// changed caller"),
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
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(i32::from(!updated)),
                "{name}, updated={updated}: {stderr}"
            );
            let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(contains(&hook, "moved", name), updated, "{hook}");
            assert_eq!(contains(&hook, bucket, name), updated, "{hook}");
            assert_eq!(contains(&hook, "rm", name), !updated, "{hook}");
        }
    }
}

#[test]
fn moved_compatible_change_hook_scopes_old_inventory_to_subdirectory() {
    for updated in [true, false] {
        let repo = TestRepo::new();
        repo.create_dir_all("pkg/src");
        repo.write("pkg/Cargo.toml", package("subdirectory", ""));
        let caller = |module: &str| {
            format!(
                "// astro-sight:allow-dead\npub fn read() -> u8 {{\n    crate::{module}::W\n}}\n"
            )
        };
        repo.write(
            "pkg/src/lib.rs",
            format!("pub mod old;\npub mod new;\n{}", caller("old")),
        );
        repo.write("pkg/src/old.rs", "pub const W: u8 = 1;\n");
        repo.write("pkg/src/new.rs", "");
        repo.write("outside.rs", "include!(\"unavailable.rs\");\n");
        repo.init_git();
        repo.commit_all("base");
        repo.git(["rm", "pkg/src/old.rs"]);
        repo.write("pkg/src/new.rs", "pub const W: u8 = 1 + 0;\n");
        repo.write(
            "pkg/src/lib.rs",
            format!(
                "pub mod new;\n{}",
                caller(if updated { "new" } else { "old" }),
            ),
        );
        let scope = repo.root().join("pkg");
        let output = cargo_bin()
            .args([
                "review",
                "--dir",
                scope.to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(i32::from(!updated)),
            "updated={updated}: {stderr}"
        );
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(contains(&hook, "moved", "W"), updated, "{hook}");
        assert_eq!(contains(&hook, "const_value", "W"), updated, "{hook}");
        assert_eq!(contains(&hook, "rm", "W"), !updated, "{hook}");
    }
}
