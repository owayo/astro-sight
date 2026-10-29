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
