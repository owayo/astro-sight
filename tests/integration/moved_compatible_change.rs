//! 宣言の移動を、hook の出力と終了コードまで確かめる。

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
fn typescript_move_checks_source_reexports() {
    for blocking in [true, false] {
        let repo = TestRepo::new();
        repo.init_git();
        let cases: &[(&str, &str)] = if blocking {
            &[
                (
                    "quoted",
                    r#"export { other as "NAME" } from './provider.js';"#,
                ),
                ("star", "export * from './provider.js';"),
                ("named", "export { NAME } from './provider.js';"),
                ("typeOnly", "export type { NAME } from './moved.js';"),
                (
                    "ambiguous",
                    "export * from './moved.js'; export * from './provider.js';",
                ),
                ("missing", "export * from './empty.js';"),
                ("missingQuoted", "export * from './empty.js';"),
            ]
        } else {
            &[
                ("quoted", r#"export { NAME as "NAME" } from './moved.js';"#),
                ("star", "export * from './moved.js';"),
                (
                    "explicit",
                    "export * from './provider.js'; export { NAME } from './moved.js';",
                ),
                (
                    "diamond",
                    "export * from './one.js'; export * from './two.js';",
                ),
                ("updated", "export * from './empty.js';"),
                ("sibling", "export * from './empty.js';"),
            ]
        };
        let declaration = |name| format!("export function {name}(): number {{ return 1; }}\n");
        let consumer = |name, path| match name {
            "missing" => {
                format!("import * as api from './{path}.js';\nexport const n = api['{name}']();\n")
            }
            "updated" => format!(
                "import * as api from './{path}.js';\nexport const n = api['\\u0075pdated']();\n"
            ),
            "sibling" => {
                format!("import * as api from './{path}.js';\nexport const n = api.other;\n")
            }
            "missingQuoted" => format!(
                "import {{ '{name}' as read }} from './{path}.js';\nexport const n = read();\n"
            ),
            _ => format!(
                "import {{ {name} }} from './{path}.js';\nexport const n: number = {name}();\n"
            ),
        };
        for &(name, _) in cases {
            repo.create_dir_all(name);
            let mut api = declaration(name);
            if name == "sibling" {
                api.push_str("export { other } from './empty.js';\n");
            }
            repo.write(format!("{name}/api.ts"), api);
            repo.write(format!("{name}/moved.ts"), "export {};\n");
            repo.write(format!("{name}/empty.ts"), "export const other = false;\n");
            repo.write(
                format!("{name}/provider.ts"),
                format!("export const other = false, {name} = false;\n"),
            );
            repo.write(format!("{name}/use.ts"), consumer(name, "api"));
            if matches!(name, "sibling" | "updated") {
                repo.write(
                    format!("{name}/direct.ts"),
                    format!(
                        "import {{ {name} }} from './api.js';\nexport const direct = {name}();\n"
                    ),
                );
            }
            for forwarder in ["one", "two"] {
                repo.write(format!("{name}/{forwarder}.ts"), "export {};\n");
            }
        }
        repo.commit_all("base");
        for &(name, source) in cases {
            repo.write(format!("{name}/api.ts"), source.replace("NAME", name));
            repo.write(format!("{name}/moved.ts"), declaration(name));
            if matches!(name, "sibling" | "updated") {
                repo.write(
                    format!("{name}/direct.ts"),
                    format!(
                        "import {{ {name} }} from './moved.js';\nexport const direct = {name}();\n"
                    ),
                );
            }
            if name == "updated" {
                repo.write(format!("{name}/use.ts"), consumer(name, "moved"));
            }
            if name == "diamond" {
                for forwarder in ["one", "two"] {
                    repo.write(
                        format!("{name}/{forwarder}.ts"),
                        "export * from './moved.js';\n",
                    );
                }
            }
        }
        for staged in [false, true] {
            if staged {
                repo.stage_all();
                for &(name, _) in cases {
                    repo.write(format!("{name}/api.ts"), declaration(name));
                }
            }
            let mut cmd = cargo_bin();
            cmd.args(["review", "--git", "--hook", "--dir"])
                .arg(repo.root());
            if staged {
                cmd.arg("--staged");
            }
            let output = cmd.output().unwrap();
            let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            for &(name, _) in cases {
                assert_eq!(
                    contains(&hook, "mod", name) || contains(&hook, "rm", name),
                    blocking,
                    "{name}, staged={staged}: {hook}"
                );
                assert_eq!(
                    contains(&hook, "moved", name),
                    !blocking,
                    "{name}, staged={staged}: {hook}"
                );
            }
            assert_eq!(
                output.status.code(),
                Some(i32::from(blocking)),
                "staged={staged}: {hook}"
            );
        }
    }
}

#[test]
fn typescript_move_uses_the_current_projects_relative_resolution() {
    let repo = TestRepo::new();
    repo.init_git();
    let decl = "export function f(): number { return 1; }\n";
    repo.write("api.ts", decl);
    repo.write("moved.ts", "export {};\n");
    repo.write("moved.alt.ts", "export const f = false;\n");
    repo.write(
        "consumer.ts",
        "import { f } from './api.js';\nexport const n = f();\n",
    );
    let ordinary = r#"{"compilerOptions":{"moduleSuffixes":[""]}}"#;
    let alternate = r#"{"compilerOptions":{"moduleSuffixes":[".alt",""]}}"#;
    repo.write("tsconfig.json", ordinary);
    repo.commit_all("base");
    repo.write("moved.ts", decl);
    repo.write("api.ts", "export * from './moved.js';\n");
    repo.write("tsconfig.json", alternate);
    repo.stage_all();
    repo.write("tsconfig.json", ordinary);
    for (staged, args) in [(false, vec!["--git"]), (true, vec!["--git", "--staged"])] {
        let review = repo.run_json("review", &args);
        let has = |bucket: &str| {
            review["api_changes"][bucket]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|r| r["name"] == "f"))
        };
        assert_eq!(has("modified"), staged, "{review}");
        assert_eq!(has("moved"), !staged, "{review}");
    }
}

#[test]
fn typescript_move_keeps_destination_name_capture_blocking() {
    let repo = TestRepo::new();
    repo.init_git();
    let read = "export function read() { return Date; }\n";
    repo.write(
        "api.ts",
        "export const Date = 1;\nexport const Label = 2;\n",
    );
    let dest = format!("{read}export function local(Label: number) {{ return Label; }}\n");
    repo.write("dest.ts", &dest);
    repo.write(
        "use.ts",
        "import { read } from './dest.js';\nexport const made = new (read())();\n",
    );
    repo.commit_all("base");
    repo.write("api.ts", "export {};\n");
    repo.write(
        "dest.ts",
        format!("export const Date = 1;\nexport const Label = 2;\n{dest}"),
    );
    for staged in [false, true] {
        if staged {
            repo.stage_all();
            repo.write("dest.ts", &dest);
        }
        let mut cmd = cargo_bin();
        cmd.args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root());
        if staged {
            cmd.arg("--staged");
        }
        let output = cmd.output().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "staged={staged}: {json}");
        assert!(contains(&json, "mod", "Date"), "{json}");
        assert!(!contains(&json, "moved", "Date"), "{json}");
        assert!(contains(&json, "moved", "Label"), "{json}");
        assert!(!contains(&json, "mod", "Label"), "{json}");
    }
}

#[test]
fn typescript_move_checks_an_existing_import_in_the_base_project() {
    for alternate in [false, true] {
        let repo = TestRepo::new();
        repo.init_git();
        let read = "export function read() { return Date; }\n";
        repo.write("api.ts", "export const Date = 1;\n");
        repo.write("api.alt.ts", "export const Date = globalThis.Date;\n");
        repo.write(
            "dest.ts",
            format!("import {{ Date }} from './api.js';\n{read}"),
        );
        repo.write("notes.md", "Before\n");
        repo.write(
            "tsconfig.json",
            if alternate {
                r#"{"compilerOptions":{"moduleSuffixes":[".alt",""]}}"#
            } else {
                "{}"
            },
        );
        repo.commit_all("base");
        repo.write("api.ts", "export {};\n");
        repo.write("dest.ts", format!("export const Date = 1;\n{read}"));
        repo.write("notes.md", "After\n");
        repo.write("tsconfig.json", "{}");
        let review = repo.run_json("review", &["--git"]);
        let has = |bucket: &str| {
            review["api_changes"][bucket]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|s| s["name"] == "Date"))
        };
        assert_eq!(has("modified"), alternate, "{review}");
        assert_eq!(has("moved"), !alternate, "{review}");
    }
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
