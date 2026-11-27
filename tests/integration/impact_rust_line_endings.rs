use super::support::*;

const API: &str = "pub const MARKER: u32 = 1;\npub struct Config;\npub enum Outcome { Ready }\npub fn run() -> u32 { MARKER }\n";

fn fixture(old: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.write("api.rs", old);
    repo.write("caller.rs", "pub fn caller() { let _ = crate::api::MARKER; let _ = crate::api::Config; let _ = crate::api::Outcome::Ready; crate::api::run(); }\n");
    repo.init_git();
    repo.git(["config", "core.autocrlf", "false"]);
    repo.commit_all("base");
    repo
}

fn origins(context: &serde_json::Value) -> Vec<&str> {
    context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| change["affected_symbols"].as_array().unwrap())
        .map(|symbol| symbol["name"].as_str().unwrap())
        .collect()
}

#[test]
fn adjacent_rust_declarations_report_only_the_changed_item() {
    let old = "pub struct Stable; pub fn update(_: u8) {}\n";
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='sample'\nversion='1.0.0'\nedition='2021'\n",
    );
    repo.write("src/lib.rs", "pub mod producer;\npub mod reader;\n");
    repo.write("src/producer.rs", old);
    repo.write("src/reader.rs", "pub fn read() {\n    let _ = crate::producer::Stable;\n    crate::producer::update(1_u8);\n}\n");
    repo.init_git();
    repo.git(["config", "core.autocrlf", "false"]);
    repo.commit_all("base");
    let changed_type = old.replace("struct Stable;", "struct Stable { pub size: u8 }");
    repo.write("src/producer.rs", old.replace("_: u8", "_: u16"));
    let check = |args: &[&str], name: &str, line: usize| {
        let context = repo.run_json("context", args);
        assert_eq!(origins(&context), [name], "{context}");
        if name == "update" {
            let signature = &context["changes"][0]["signature_changes"][0];
            assert_eq!(signature["name"], "update");
            assert!(
                signature["old_signature"]
                    .as_str()
                    .unwrap()
                    .contains("_: u8")
            );
            assert!(
                signature["new_signature"]
                    .as_str()
                    .unwrap()
                    .contains("_: u16")
            );
        }
        let output = cargo_bin()
            .arg("impact")
            .arg("--dir")
            .arg(repo.root())
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        let callers: Vec<_> = stderr
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('→'))
            .collect();
        assert_eq!(callers, [format!("→ src/reader.rs:{line} [{name}]")]);
    };
    check(&["--git"], "update", 3);
    repo.stage_all();
    repo.write("src/producer.rs", changed_type);
    check(&["--git", "--staged"], "update", 3);
    check(&["--git"], "Stable", 2);
}

#[test]
fn adjacent_rust_declarations_display_the_changed_function_signature() {
    let before = "pub fn idle() {} pub fn update(_: u8) {}";
    let repo = fixture(&format!("{before}\n"));
    repo.write("api.rs", format!("{}\n", before.replace("u8", "u16")));
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(origins(&context), ["update"], "{context}");
    let change = &context["changes"][0]["signature_changes"][0];
    assert_eq!(change["name"], "update");
    assert_eq!(change["old_signature"], "pub fn update(_: u8) {}");
    assert_eq!(change["new_signature"], "pub fn update(_: u16) {}");
}

#[test]
fn adjacent_rust_declarations_keep_a_type_supplied_by_the_including_module() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='probe'\nversion='0.1.0'\nedition='2021'\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod producer { type u8 = Base; include!(\"producer.rs\"); }\n",
    );
    let old = "pub type Base = u16; pub struct Stable(pub u8);\n";
    repo.write("src/producer.rs", old);
    repo.init_git();
    repo.git(["config", "core.autocrlf", "false"]);
    repo.commit_all("base");
    repo.write("src/producer.rs", old.replace("u16", "u32"));
    let context = repo.run_json("context", &["--git"]);
    assert!(origins(&context).contains(&"Stable"), "{context}");
}

#[test]
fn adjacent_rust_declarations_keep_dependencies_and_attribute_changes() {
    for (old, new, affected) in [
        (
            "pub fn update(_: u8) {} pub struct Stable;",
            "pub fn update(_: u16) {} pub struct Stable;",
            false,
        ),
        (
            "pub struct Stable;",
            "pub struct Stable; pub fn update() {}",
            false,
        ),
        (
            "pub fn update() {} pub struct Stable;",
            "pub struct Stable;",
            false,
        ),
        (
            "pub struct Stable; pub fn update() {}",
            "pub struct Stable; pub const ADDED: u8 = 1; pub fn update() {}",
            false,
        ),
        (
            "pub struct Stable; pub const GONE: u8 = 1; pub fn update() {}",
            "pub struct Stable; pub fn update() {}",
            false,
        ),
        (
            "pub struct Stable(pub u8); pub fn update() {}",
            "pub struct Stable(pub u8); pub fn update(_: u8) {}",
            true,
        ),
        (
            "pub struct Stable(pub Data); pub type Data = u8;",
            "pub struct Stable(pub Data); pub type Data = u16;",
            true,
        ),
        (
            "pub struct Stable(pub [u8; N]); pub const N: usize = 1;",
            "pub struct Stable(pub [u8; N]); pub const N: usize = 2;",
            true,
        ),
        (
            "#[derive(Clone)] pub struct Stable; pub fn update() {}",
            "pub struct Stable; pub fn update() {}",
            true,
        ),
        (
            "pub struct Stable; pub fn update() {}",
            "#[cfg(any())] pub struct Stable; pub fn update() {}",
            true,
        ),
        (
            "type u8 = Base;\npub type Base = u16; pub struct Stable(pub u8);",
            "type u8 = Base;\npub type Base = u32; pub struct Stable(pub u8);",
            true,
        ),
        (
            "pub struct Stable; pub fn update(_: u8) {} implement!(Stable, column!());",
            "pub struct Stable; pub fn update(_: u16) {} implement!(Stable, column!());",
            true,
        ),
        (
            "pub mod left { pub struct Stable; pub fn update() {} }",
            "pub mod right { pub struct Stable; pub fn update(_: u8) {} }",
            true,
        ),
        (
            "pub mod left { pub struct Stable; } pub mod right { pub struct Stable; }",
            "pub mod left { pub struct Stable; } pub mod right { pub struct Stable(pub u8); }",
            true,
        ),
        (
            "#[project::expand] pub mod nested { pub struct Stable; pub fn update() {} }",
            "#[project::expand] pub mod nested { pub struct Stable; pub fn update(_: u8) {} }",
            true,
        ),
    ] {
        let repo = fixture(&format!("{old}\n"));
        repo.write("api.rs", format!("{new}\n"));
        let context = repo.run_json("context", &["--git"]);
        assert_eq!(
            origins(&context).contains(&"Stable"),
            affected,
            "{old} -> {new}: {context}"
        );
    }
}

#[test]
fn impact_rust_line_endings_only_have_no_origins_or_unresolved_callers() {
    for reverse in [false, true] {
        for staged in [false, true] {
            let crlf = API.replace('\n', "\r\n");
            let (old, new) = if reverse {
                (crlf.as_str(), API)
            } else {
                (API, crlf.as_str())
            };
            let repo = fixture(old);
            repo.write("api.rs", new);
            if staged {
                repo.stage_all();
                // index の証拠を作業ツリーの別変更で置き換えない。
                repo.write(
                    "api.rs",
                    API.replace("pub struct Config;", "pub struct Config { pub value: u32 }"),
                );
            }
            let args: &[&str] = if staged {
                &["--git", "--staged"]
            } else {
                &["--git"]
            };
            let context = repo.run_json("context", args);
            assert!(origins(&context).is_empty(), "{context}");
            let output = cargo_bin()
                .args(["impact", "--dir"])
                .arg(repo.root())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn impact_rust_line_endings_external_diff_and_review_use_the_same_proof() {
    let repo = fixture(API);
    repo.write("api.rs", API.replace('\n', "\r\n"));
    let diff = std::process::Command::new("git")
        .args(["diff", "--no-ext-diff", "--no-color"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(diff.status.success());
    repo.write("change.patch", diff.stdout);
    let patch_path = repo.path("change.patch");
    let context = repo.run_json("context", &["--diff-file", patch_path.to_str().unwrap()]);
    assert!(origins(&context).is_empty(), "{context}");
    let review = repo.run_json("review", &["--git"]);
    assert!(origins(&review["impact"]).is_empty(), "{review}");
}

#[test]
fn impact_rust_line_endings_keep_actual_changes_in_a_mixed_diff() {
    for (before, after, expected) in [
        ("u32 = 1", "u32 = 2", "MARKER"),
        ("run()", "run(input: u32)", "run"),
        (
            "struct Config;",
            "struct Config { pub value: u32 }",
            "Config",
        ),
    ] {
        let repo = fixture(API);
        repo.write("api.rs", API.replace(before, after).replace('\n', "\r\n"));
        let context = repo.run_json("context", &["--git"]);
        assert_eq!(origins(&context), [expected], "{context}");
        let output = cargo_bin()
            .args(["impact", "--dir"])
            .arg(repo.root())
            .arg("--git")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{expected}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn impact_rust_line_endings_preserve_literal_contents_and_scope_changes() {
    for (old, new, unchanged) in [
        (
            "pub const TEXT: &str = r#\"a\nb\"#;\n",
            "pub const TEXT: &str = r#\"a\r\nb\"#;\r\n",
            true,
        ),
        (
            "pub const TEXT: &str = \"a\\nb\";\n",
            "pub const TEXT: &str = \"a\\r\\nb\";\r\n",
            false,
        ),
        (
            "pub const TEXT: &str = r#\"a\nb\"#;\n",
            "pub const TEXT: &str = r#\"a\r\n\r\nb\"#;\r\n",
            false,
        ),
        (
            "pub mod first { pub struct Config; }\n",
            "pub mod second { pub struct Config; }\r\n",
            false,
        ),
    ] {
        let repo = fixture(old);
        repo.write("api.rs", new);
        let context = repo.run_json("context", &["--git"]);
        assert_eq!(origins(&context).is_empty(), unchanged, "{context}");
    }
}
