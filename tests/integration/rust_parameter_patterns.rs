//! Rust の引数パターン変更を呼び出し契約の変更から区別する (Issue #92)。

use super::support::{TestRepo, cargo_bin};

#[test]
fn rust_signature_formatting_does_not_block_hooks() {
    let compact = "pub fn record(owner: &str, amount: u64, memo: String, tag: String) -> Result<(), String> {\n    let _ = (owner, amount, memo, tag);\n    Ok(())\n}\n";
    let wrapped = "pub fn record(\n    owner: &str,\n    amount: u64,\n    memo: String,\n    tag: String,\n) -> Result<(), String> {\n    let _ = (owner, amount, memo, tag);\n    Ok(())\n}\n";
    let newline_only = wrapped.replace("tag: String,\n", "tag: String\n");
    let comma_only = compact.replace("tag: String)", "tag: String,)");
    for (old, new) in [
        (compact, wrapped),
        (compact, newline_only.as_str()),
        (compact, comma_only.as_str()),
        (wrapped, compact),
    ] {
        check_signature_formatting_change(old, new, false);
    }

    let generic = compact
        .replace("record(", "record<T: AsRef<str>>(")
        .replace("memo: String, tag: String", "memo: T, tag: T");
    let generic_wrapped = generic
        .replace(
            "record<T: AsRef<str>>(",
            "record<\n    T: AsRef<str,>,\n>(\n",
        )
        .replace("tag: T)", "tag: T,\n)")
        .replace("Result<(), String>", "Result<(), String,>");
    check_signature_formatting_change(&generic, &generic_wrapped, false);
    let comment = compact.replace("tag: String)", "tag: String // tag\n)");
    let comment_comma = compact.replace("tag: String)", "tag: String, // tag\n)");
    check_signature_formatting_change(&comment, &comment_comma, false);
    check_signature_formatting_change(
        "pub fn record<T>() -> Result<T, String> {\n    Err(String::new())\n}\n",
        "pub fn record<\n T,\n>() -> Result<T, String,> {\n    Err(String::new())\n}\n",
        false,
    );
}

#[test]
fn rust_signature_formatting_keeps_real_contract_changes_blocking() {
    let compact = "pub fn record(owner: &str, amount: u64, memo: String, tag: String) -> Result<(), String> {\n    let _ = (owner, amount, memo, tag);\n    Ok(())\n}\n";
    for new in [
        compact.replace("tag: String)", "tag: String, extra: bool,\n)"),
        compact.replace("memo: String, ", ""),
        compact.replace("amount: u64", "amount: u32"),
        compact.replace("owner: &str, amount: u64", "amount: u64, owner: &str"),
        compact.replace("Result<(), String>", "Result<u64, String>"),
    ] {
        check_signature_formatting_change(compact, &new, true);
    }
    let parenthesized = compact.replace("tag: String)", "tag: (String))");
    let tuple = compact.replace("tag: String)", "tag: (String,))");
    check_signature_formatting_change(&parenthesized, &tuple, true);
    let attributed = format!("#[custom]\n{compact}");
    let attributed_wrapped = attributed.replace("tag: String)", "tag: String,\n)");
    check_signature_formatting_change(&attributed, &attributed_wrapped, true);
    let attributed_comma = attributed.replace("tag: String)", "tag: String,)");
    check_signature_formatting_change(&attributed, &attributed_comma, true);
}

fn check_signature_formatting_change(old: &str, new: &str, blocking: bool) {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"ledger\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod book;\npub mod api;\n");
    repo.write("src/book.rs", old);
    repo.write("src/api.rs", if old.contains("record<T>()") {
        "use crate::book;\npub fn handle() -> Result<(), String> {\n    book::record::<()>()\n}\n"
    } else {
        "use crate::book;\npub fn handle() -> Result<(), String> {\n    book::record(\"a\", 1, String::new(), String::new())\n}\n"
    });
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/book.rs", new);
    for command in ["impact", "review"] {
        let output = cargo_bin()
            .args([command, "--git", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(i32::from(blocking)),
            "{command}: {old} -> {new}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if command == "review" && blocking {
            let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert!(
                json["api"]["mod"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["n"] == "record")),
                "{json}"
            );
            assert!(
                json["impacts"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()),
                "{json}"
            );
        }
    }
}

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
