//! 同名の別定義だけで、実際のメソッド呼び出し元を除外しない。

use super::support::*;

#[derive(Clone, Copy, Debug)]
enum Definition {
    Foreign,
    Local,
    Unrelated,
}

const CPP_BEFORE: &str = "class Tank {\npublic:\n    int feed() { return 0; }\n};\n";
const CPP_AFTER: &str = "class Tank {\npublic:\n    int feed(int amount) { return amount; }\n};\n";
const RUST_BEFORE: &str =
    "pub struct Tank;\nimpl Tank { pub fn feed(&self) -> i32 { 0 } }\nmod consumer;\nmod other;\n";
const RUST_AFTER: &str = "pub struct Tank;\nimpl Tank { pub fn feed(&self, amount: i32) -> i32 { amount } }\nmod consumer;\nmod other;\n";

fn cpp_repo(definition: Definition) -> TestRepo {
    let repo = TestRepo::new();
    repo.write("tank.hpp", CPP_BEFORE);
    repo.write(
        "consumer.cpp",
        "#include \"tank.hpp\"\nint run() { Tank tank; return tank.feed(); }\n",
    );
    match definition {
        Definition::Foreign => repo.write("other.py", "def feed(value):\n    return value\n"),
        Definition::Local => repo.write("other.cpp", "int feed(int value) { return value; }\n"),
        Definition::Unrelated => repo.write("other.py", "def separate(value):\n    return value\n"),
    }
    repo.init_git();
    repo.commit_all("initial");
    repo
}

fn rust_repo(definition: Definition) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname=\"consumer\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
    );
    repo.write("src/lib.rs", RUST_BEFORE);
    repo.write(
        "src/consumer.rs",
        "use crate::Tank;\npub fn run(tank: &Tank) -> i32 { tank.feed() }\n",
    );
    repo.write(
        "src/other.rs",
        if matches!(definition, Definition::Local) {
            "pub fn feed(value: i32) -> i32 { value }\n"
        } else {
            "pub fn separate(value: i32) -> i32 { value }\n"
        },
    );
    if matches!(definition, Definition::Foreign) {
        repo.write("other.py", "def feed(value):\n    return value\n");
    }
    repo.init_git();
    repo.commit_all("initial");
    repo
}

fn has_caller(result: &serde_json::Value, source: &str, path: &str, symbol: &str) -> bool {
    result["changes"].as_array().is_some_and(|changes| {
        changes.iter().any(|change| {
            change["path"] == source
                && change["impacted_callers"]
                    .as_array()
                    .is_some_and(|callers| {
                        callers.iter().any(|caller| {
                            caller["path"] == path
                                && caller["symbols"]
                                    .as_array()
                                    .is_some_and(|names| names.iter().any(|name| name == symbol))
                        })
                    })
        })
    })
}

fn assert_callers(
    repo: &TestRepo,
    source: &str,
    symbol: &str,
    expected: &[&str],
    excluded: &[&str],
    staged: bool,
) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let context = repo.run_json("context", args);
    for path in expected {
        assert!(has_caller(&context, source, path, symbol), "{context}");
    }
    for path in excluded {
        assert!(!has_caller(&context, source, path, symbol), "{context}");
    }
    assert!(context.get("truncations").is_none(), "{context}");
    let review = repo.run_json("review", args);
    for path in expected {
        assert!(
            has_caller(&review["impact"], source, path, symbol),
            "{review}"
        );
    }
    for path in excluded {
        assert!(
            !has_caller(&review["impact"], source, path, symbol),
            "{review}"
        );
    }
    assert!(review.get("truncations").is_none(), "{review}");
    let output = cargo_bin()
        .env("__CARGO_TEST_ROOT", repo.root().canonicalize().unwrap())
        .args(["impact", "--dir"])
        .arg(repo.root())
        .args(args)
        .output()
        .expect("impact");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr).replace('\\', "/");
    for path in expected {
        assert!(stderr.contains(path), "{stderr}");
    }
    let refs = repo.run_json("refs", &["--name", symbol]);
    for path in expected.iter().chain(excluded) {
        assert!(
            refs["refs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reference| { reference["path"] == *path && reference["kind"] == "ref" }),
            "{refs}"
        );
    }
}

fn cpp_competing_definition(definition: Definition, staged: bool) {
    let repo = cpp_repo(definition);
    repo.write("tank.hpp", CPP_AFTER);
    if staged {
        repo.stage_all();
        repo.write("tank.hpp", CPP_BEFORE);
    }
    assert_callers(&repo, "tank.hpp", "feed", &["consumer.cpp"], &[], staged);
}

fn rust_competing_definition(definition: Definition, staged: bool) {
    let repo = rust_repo(definition);
    repo.write("src/lib.rs", RUST_AFTER);
    if staged {
        repo.stage_all();
        repo.write("src/lib.rs", RUST_BEFORE);
    }
    assert_callers(
        &repo,
        "src/lib.rs",
        "feed",
        &["src/consumer.rs"],
        &[],
        staged,
    );
}

#[test]
fn cpp_competing_definitions_preserve_method_callers() {
    cpp_competing_definition(Definition::Foreign, false);
}

#[test]
fn cpp_competing_definitions_preserve_index_method_callers() {
    cpp_competing_definition(Definition::Foreign, true);
}

#[test]
fn rust_competing_definitions_preserve_public_method_callers() {
    rust_competing_definition(Definition::Foreign, false);
}

#[test]
fn rust_competing_definitions_preserve_index_public_method_callers() {
    rust_competing_definition(Definition::Foreign, true);
}

#[test]
fn cpp_same_language_competing_definitions_preserve_method_callers() {
    cpp_competing_definition(Definition::Local, false);
}

#[test]
fn cpp_same_language_competing_definitions_preserve_index_method_callers() {
    cpp_competing_definition(Definition::Local, true);
}

#[test]
fn rust_same_language_competing_definitions_preserve_public_method_callers() {
    rust_competing_definition(Definition::Local, false);
}

#[test]
fn rust_same_language_competing_definitions_preserve_index_public_method_callers() {
    rust_competing_definition(Definition::Local, true);
}

#[test]
fn unrelated_definitions_preserve_method_callers() {
    let cpp = cpp_repo(Definition::Unrelated);
    cpp.write("tank.hpp", CPP_AFTER);
    assert_callers(&cpp, "tank.hpp", "feed", &["consumer.cpp"], &[], false);
    let rust = rust_repo(Definition::Unrelated);
    rust.write("src/lib.rs", RUST_AFTER);
    assert_callers(
        &rust,
        "src/lib.rs",
        "feed",
        &["src/consumer.rs"],
        &[],
        false,
    );
}

#[test]
fn python_competing_definitions_preserve_callers_and_proven_other_bindings() {
    for owner in ["Trail", "_Trail"] {
        let repo = TestRepo::new();
        let before = format!("class {owner}:\n    def join(self, part):\n        return part\n");
        repo.write("owner.py", &before);
        repo.write(
            "consumer.py",
            format!("from owner import {owner}\ndef run():\n    return {owner}().join(\"x\")\n"),
        );
        repo.write(
            "unknown.py",
            "def run(value):\n    return value.join(\"x\")\n",
        );
        repo.write(
            "other.py",
            "def factory():\n    def join(value):\n        return value\n    return join\n",
        );
        repo.write(
            "disjoint.py",
            "import os.path\ndef run(part):\n    return os.path.join(part)\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "owner.py",
            before.replace("self, part", "self, part, origin"),
        );
        assert_callers(
            &repo,
            "owner.py",
            "join",
            &["consumer.py", "unknown.py"],
            &["disjoint.py"],
            false,
        );
    }
}
