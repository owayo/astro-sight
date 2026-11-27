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
            "from shared import decorate\n",
            "async def decorate(value: str) -> str:\n    return '<' + value + '>'\n",
            "rm",
        ),
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

#[test]
fn python_local_function_alias_preserves_name_with_unverified_compatibility() {
    let repo = TestRepo::new();
    repo.init_git();
    let original = "def total(x):\n    return x + 1\n";
    let renamed = original.replace("total", "sum_values");
    let caller = "from api import total\n\ndef run():\n    return total(2)\n";
    repo.write("api.py", original);
    repo.write("caller.py", caller);
    repo.write("legacy.py", "from api import total\nassert total(2) == 3\n");
    repo.commit_all("base");
    repo.write("caller.py", caller.replace("total", "sum_values"));
    repo.write("api.py", format!("{renamed}\ntotal = sum_values\n"));

    let assert_row = |staged, expected| {
        let output = hook(&repo, staged);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        let result: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
        let has_old = |bucket: &str| {
            result["api"][bucket].as_array().is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry["n"] == "total" && entry["f"] == "api.py")
            })
        };
        assert_eq!((has_old("mod"), has_old("rm")), expected, "{result}");
        assert!(!has_old("mod_compat"), "{result}");
    };
    let json = repo.run_json("review", &["--git"]);
    let api = &json["api_changes"];
    assert!(
        api["modified"].as_array().is_some_and(|entries| entries
            .iter()
            .any(|entry| entry["name"] == "total"
                && entry["new_signature"] == "def sum_values(x):"
                && entry["unverified"]["reason"] == "compatibility_unverified")),
        "{api}"
    );
    assert!(
        api["import_rebindings"]
            .as_array()
            .is_none_or(|entries| entries.is_empty()),
        "{api}"
    );
    assert_row(false, (true, false));
    repo.stage_all();
    for body in [
        renamed.clone(),
        format!("{renamed}\nif False:\n    total = sum_values\n"),
        format!("{renamed}\ntotal = sum_values\ntotal = 0\n"),
        format!("{renamed}\ntotal = sum_values\ndel total\n"),
        format!("@decorate\n{renamed}\ntotal = sum_values\n"),
        format!("{renamed}\ntotal = sum_values\nglobals().pop('total')\n"),
        format!("total = sum_values\n{renamed}"),
        format!("{renamed}\ntotal = sum_values\n\u{ff54}otal = 0\n"),
    ] {
        repo.write("api.py", body);
        assert_row(false, (false, true));
    }
    assert_row(true, (true, false));
    repo.write("api.py", format!("{renamed}\ntotal = (sum_values)\n"));
    assert_row(false, (true, false));
    for target in [
        renamed.replace("x + 1", "x + 9"),
        renamed.replace("(x)", "(x, y)"),
    ] {
        repo.write("api.py", format!("{target}\ntotal = sum_values\n"));
        assert_row(false, (true, false));
    }
}

#[test]
fn python_local_alias_does_not_extend_import_equivalence() {
    let repo = fixture();
    repo.write("api.py", "from shared import decorate\n");
    for suffix in [
        "alias = decorate\n",
        "alias = decorate\nexec('del decorate')\n",
    ] {
        repo.write("shared.py", format!("{FUNCTION}\n{suffix}"));
        let output = hook(&repo, false);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let result: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
        assert_eq!(output.status.code(), Some(1), "{result}");
        assert!(
            result["api"]["rm"].as_array().is_some_and(|entries| entries
                .iter()
                .any(|entry| entry["f"] == "api.py" && entry["n"] == "decorate")),
            "{result}"
        );
    }
}

#[test]
fn python_local_async_function_alias_distinguishes_retention_and_removal() {
    let repo = TestRepo::new();
    repo.init_git();
    let old = "async def total(x):\n    return x + 1\n";
    let renamed = old.replace("total", "sum_values");
    repo.write("api.py", old);
    repo.write(
        "legacy.py",
        "from api import total\nfrom asyncio import run\nassert run(total(2)) == 3\n",
    );
    repo.commit_all("base");

    for (body, bucket) in [
        (format!("{renamed}\ntotal = sum_values\n"), "mod"),
        (
            format!("{}\ntotal = sum_values\n", renamed.replace("(x)", "(x, y)")),
            "mod",
        ),
        (renamed, "rm"),
    ] {
        repo.write("api.py", body);
        let output = hook(&repo, false);
        let result: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{result}");
        assert!(
            result["api"][bucket].as_array().is_some_and(|rows| rows
                .iter()
                .any(|row| row["n"] == "total" && row["f"] == "api.py")),
            "{result}"
        );
    }
}
