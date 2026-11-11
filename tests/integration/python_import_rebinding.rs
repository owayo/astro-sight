use super::support::*;

const FUNCTION: &str = "def decorate(value: str) -> str:\n    return '<' + value + '>'\n";

fn fixture() -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "api.py",
        format!("{FUNCTION}\ndef own(value: str) -> str:\n    return decorate(value)\n"),
    );
    repo.write("shared.py", FUNCTION);
    repo.write(
        "client.py",
        "from api import decorate\nprint(decorate('x'))\n",
    );
    repo.commit_all("base");
    repo
}

fn hook(repo: &TestRepo, staged: bool) -> std::process::Output {
    let mut command = cargo_bin_with_explicit_config();
    command.args([
        "review",
        "--dir",
        repo.root().to_str().unwrap(),
        "--git",
        "--hook",
    ]);
    if staged {
        command.arg("--staged");
    }
    command.output().unwrap()
}

#[test]
fn python_import_rebinding_hook_context_staged_and_refs_preserve_proof_boundaries() {
    let after = "from shared import decorate as decorate\n\ndef own(value: str) -> str:\n    return decorate(value)\n";
    let repo = fixture();
    repo.write("api.py", after);
    let json = repo.run_json("review", &["--git"]);
    assert_eq!(
        json["api_changes"]["import_rebindings"][0]["target"]["file"], "shared.py",
        "{json}"
    );
    assert_eq!(
        json["api_changes"]["import_rebindings"][0]["equivalence"], "proven",
        "{json}"
    );
    assert_eq!(
        json["api_changes"]["import_rebindings"][0]["same_file_uses"][0]["line"], 3,
        "{json}"
    );
    assert_eq!(
        json["api_changes"]["import_rebindings"][0]["same_file_uses"][0]["binding_verified"],
        true
    );
    let refs = repo.run_json("refs", &["--name", "decorate"]);
    assert!(
        refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|site| site["path"] == "api.py")
            .count()
            >= 3,
        "{refs}"
    );
    let output = hook(&repo, false);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let hook_json: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(
        hook_json["api"]["import_rebindings"][0]["equivalence"], "proven",
        "{hook_json}"
    );
    assert!(
        hook_json["blocking_categories"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{hook_json}"
    );
    let toon = cargo_bin_with_explicit_config()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--format",
            "toon",
        ])
        .output()
        .unwrap();
    assert!(toon.status.success());
    assert!(String::from_utf8_lossy(&toon.stdout).contains("import_rebindings"));
    repo.stage_all();
    repo.write("api.py", "");
    let staged = hook(&repo, true);
    assert!(
        staged.status.success(),
        "{}",
        String::from_utf8_lossy(&staged.stderr)
    );
    let working = hook(&repo, false);
    assert!(
        !working.status.success(),
        "{}",
        String::from_utf8_lossy(&working.stderr)
    );
}

#[test]
fn python_import_rebinding_hook_distinguishes_unverified_binding_and_true_removal() {
    for (replacement, target, bucket) in [
        (
            "from shared import decorate as decorate\n",
            "def decorate(value: str, suffix: str) -> str:\n    return value + suffix\n",
            "mod",
        ),
        ("", FUNCTION, "rm"),
        (
            "if True:\n    from shared import decorate as decorate\n",
            FUNCTION,
            "rm",
        ),
        (
            "def load():\n    from shared import decorate as decorate\n",
            FUNCTION,
            "rm",
        ),
    ] {
        let repo = fixture();
        repo.write("shared.py", target);
        repo.write("api.py", replacement);
        let output = hook(&repo, false);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        let json: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
        assert!(
            json["api"][bucket]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == "decorate")),
            "{json}"
        );
        if bucket == "mod" {
            assert_eq!(
                json["api"]["import_rebindings"][0]["equivalence"], "unverified",
                "{json}"
            );
        }
    }
}
