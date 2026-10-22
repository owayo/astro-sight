//! Rust の引数パターン変更を呼び出し契約の変更から区別する (Issue #92)。

use super::support::{TestRepo, cargo_bin};

fn check_change(old: &str, new: &str, blocking: bool) {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let argument = if old.contains("(a, b)") {
        "(1, 2)"
    } else if old.contains("&mut i32") {
        "&mut 1"
    } else if old.contains("Value") {
        "crate::One"
    } else {
        "1"
    };
    repo.write("src/lib.rs", format!("pub mod ops;\npub struct One;\npub struct Two;\npub fn run() {{ let _ = ops::double({argument}); }}\n"));
    repo.write("src/ops.rs", old);
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/ops.rs", new);
    for command in ["impact", "review"] {
        let output = cargo_bin()
            .args([command, "--git", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(i32::from(blocking)),
            "{command}: {old} -> {new}: {stderr}"
        );
        if command == "review" && blocking {
            let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert!(
                json["api"]["mod"]
                    .as_array()
                    .is_some_and(|changes| changes.iter().any(|c| c["n"] == "double")),
                "{json}"
            );
        }
    }
}

#[test]
fn rust_parameter_patterns_do_not_block_hooks() {
    for (old, new) in [
        (
            "pub fn double(_n: i32) -> i32 {\n    0\n}\n",
            "pub fn double(n: i32) -> i32 {\n    0\n}\n",
        ),
        (
            "pub fn double((a, b): (i32, i32)) -> i32 {\n    a + b\n}\n",
            "pub fn double((x, y): (i32, i32)) -> i32 {\n    x + y\n}\n",
        ),
        (
            "pub fn double(ref n: i32) -> i32 { 0 }\n",
            "pub fn double(n: i32) -> i32 { n * 2 }\n",
        ),
        (
            "pub fn double(\n    _n: i32,\n) -> i32 {\n    0\n}\n",
            "pub fn double(\n    n: i32,\n) -> i32 {\n    n * 2\n}\n",
        ),
        (
            "#[inline]\npub fn double(_n: i32) -> i32 { 0 }\n",
            "#[inline]\npub fn double(n: i32) -> i32 { n * 2 }\n",
        ),
        (
            "pub fn double(_: i32) -> i32 { 0 }\n",
            "pub fn double(n: i32) -> i32 { n * 2 }\n",
        ),
    ] {
        check_change(old, new, false);
    }
}

#[test]
fn rust_parameter_contract_and_macro_attributes_remain_blocking() {
    for (old, new) in [
        (
            "pub fn double(_n: i32) -> i32 { 0 }\n",
            "pub fn double(n: i64) -> i32 { 0 }\n",
        ),
        (
            "pub fn double(_n: &mut i32) -> i32 { 0 }\n",
            "pub fn double(n: &i32) -> i32 { 0 }\n",
        ),
        (
            "pub fn double(_n: i32) -> i32 { 0 }\n",
            "pub fn double(n: i32, other: i32) -> i32 { 0 }\n",
        ),
        (
            "pub fn double(_n: i32) -> i32 { 0 }\n",
            "pub fn double(n: i32) -> i64 { 0 }\n",
        ),
        (
            "pub extern \"C\" fn double(_n: i32) -> i32 { 0 }\n",
            "pub extern \"C-unwind\" fn double(n: i32) -> i32 { 0 }\n",
        ),
        (
            "use crate::One as Value;\npub fn double(_n: Value) {}\n",
            "use crate::Two as Value;\npub fn double(n: Value) {}\n",
        ),
    ] {
        check_change(old, new, true);
    }
    for attr in [
        "pyfunction",
        "tauri::command",
        "wasm_bindgen",
        "cfg_attr(feature = \"python\", pyfunction)",
    ] {
        check_change(
            &format!("#[{attr}]\npub fn double(_n: i32) -> i32 {{ 0 }}\n"),
            &format!("#[{attr}]\npub fn double(n: i32) -> i32 {{ 0 }}\n"),
            true,
        );
    }
}
