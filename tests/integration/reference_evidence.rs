use super::support::*;

#[test]
fn reference_evidence_bash_dotted_names_keep_test_reference_positions() {
    let repo = TestRepo::new();
    repo.create_dir_all("tests");
    repo.write("tools.sh", "utils.log() { :; }\n");
    repo.write("tests/check.sh", "utils.log\n");
    let value = repo.run_json("dead-code", &[]);
    assert!(
        value["dead_symbols"].as_array().unwrap().is_empty(),
        "{value}"
    );
    let symbols = value["test_only_symbols"].as_array().unwrap();
    assert_eq!(symbols.len(), 1, "{value}");
    assert_eq!(symbols[0]["name"], "utils.log");
    let evidence = &symbols[0]["reference_evidence"];
    assert_eq!(evidence["scope"]["complete"], true);
    assert_eq!(evidence["references"]["total"], 1);
    assert_eq!(evidence["references"]["test_total"], 1);
    assert_eq!(evidence["references"]["sites"][0]["file"], "tests/check.sh");
    assert_eq!(evidence["references"]["sites"][0]["line"], 0);
    assert_eq!(evidence["references"]["sites"][0]["column"], 0);
}

#[test]
fn reference_evidence_test_only_keeps_references_and_external_entry_separate() {
    let repo = TestRepo::new();
    repo.create_dir_all("tests");
    repo.write(
        "calc.py",
        "def squared(value: int) -> int:\n    return value ** 2\n",
    );
    repo.write(
        "tests/test_calc.py",
        "from calc import squared\ndef test_squared():\n    assert squared(2) == 4\n",
    );
    repo.write(
        "pyproject.toml",
        "[project.scripts]\nalpha = 'calc:squared'\nbeta = 'calc:squared'\n",
    );
    let value = repo.run_json("dead-code", &[]);
    assert!(
        value["dead_symbols"].as_array().unwrap().is_empty(),
        "{value}"
    );
    let symbols = value["test_only_symbols"].as_array().unwrap();
    assert_eq!(symbols.len(), 1, "{value}");
    let symbol = &symbols[0];
    assert_eq!(symbol["name"], "squared");
    let evidence = &symbol["reference_evidence"];
    assert_eq!(evidence["classification"], "test_references_only");
    assert_eq!(evidence["scope"]["root"], ".");
    assert_eq!(evidence["scope"]["complete"], true);
    assert_eq!(evidence["references"]["production_total"], 0);
    assert_eq!(evidence["references"]["test_total"], 2);
    assert_eq!(
        evidence["references"]["sites"][0]["file"],
        "tests/test_calc.py"
    );
    assert_eq!(evidence["references"]["sites"][0]["line"], 0);
    assert_eq!(evidence["references"]["sites"][1]["line"], 2);
    assert_eq!(evidence["external_entry"]["status"], "proven");
    assert_eq!(
        evidence["external_entry"]["registrations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let toon = cargo_bin()
        .args(["dead-code", "--dir"])
        .arg(repo.root())
        .args(["--format", "toon"])
        .output()
        .unwrap();
    assert!(toon.status.success());
    let toon = String::from_utf8(toon.stdout).unwrap();
    assert!(toon.contains("reference_evidence"), "{toon}");
    assert!(toon.contains("test_references_only"), "{toon}");
    repo.remove_file("pyproject.toml");
    let missing = repo.run_json("dead-code", &[]);
    assert_eq!(missing["test_only_symbols"][0]["name"], "squared");
    assert_eq!(
        missing["test_only_symbols"][0]["reference_evidence"]["external_entry"]["status"],
        "unproven"
    );
}

#[test]
fn reference_evidence_no_callers_preserves_hook_blocking_and_metadata_is_informational() {
    let repo = TestRepo::new();
    repo.create_dir_all("pkg");
    repo.write("pkg/__init__.py", "");
    repo.write("pkg/api.py", "def entry(value):\n    return value\n");
    repo.write(
        "pyproject.toml",
        "[project.scripts]\ndemo = 'pkg.api:entry'\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("pkg/api.py", "def entry(value) -> int:\n    return value\n");
    let review = repo.run_json("review", &["--git"]);
    let change = review["api_changes"]["modified"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["name"] == "entry")
        .unwrap();
    assert_eq!(change["no_resolved_internal_callers"], true, "{review}");
    assert_eq!(
        change["reference_evidence"]["classification"],
        "no_resolved_direct_callers"
    );
    assert_eq!(
        change["reference_evidence"]["external_entry"]["status"],
        "proven"
    );
    assert!(
        review["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "entry")
    );
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        hook["blocking_categories"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("api.mod")),
        "{hook}"
    );
    assert_eq!(hook["api"]["mod"][0]["no_callers"], true);
    assert_eq!(
        hook["api"]["mod"][0]["reference_evidence"]["external_entry"]["status"],
        "proven"
    );
    assert_eq!(
        hook["dead"][0]["reference_evidence"]["classification"],
        "no_references_in_scope"
    );
    repo.stage_all();
    repo.write(
        "pyproject.toml",
        "[project.scripts]\ndemo = 'pkg.api:other'\n",
    );
    let staged = repo.run_json("review", &["--git", "--staged"]);
    assert_eq!(
        staged["api_changes"]["modified"][0]["reference_evidence"]["external_entry"]["status"],
        "proven"
    );
    let worktree = repo.run_json("review", &["--git"]);
    assert_eq!(
        worktree["api_changes"]["modified"][0]["reference_evidence"]["external_entry"]["status"],
        "unproven"
    );
}

#[test]
fn reference_evidence_hook_reports_test_only_without_new_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("tests");
    repo.write(
        "calc.py",
        "def squared(value: int) -> int:\n    return value * value\n",
    );
    repo.write(
        "tests/test_calc.py",
        "from calc import squared\ndef test_squared():\n    assert squared(2) == 4\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "calc.py",
        "def squared(value: int) -> int:\n    return value ** 2\n",
    );
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(hook["blocking_categories"], serde_json::json!([]));
    assert_eq!(
        hook["test_only"][0]["reference_evidence"]["classification"],
        "test_references_only"
    );
}

#[test]
fn reference_evidence_incomplete_scope_does_not_claim_complete_zero() {
    let repo = TestRepo::new();
    repo.write("calc.py", "def squared(value):\n    return value * value\n");
    repo.write(
        "screen.vue",
        "<script>import { squared } from './calc'; squared(2)</script>\n",
    );
    let value = repo.run_json("dead-code", &[]);
    let dead = value["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "squared")
        .unwrap();
    let evidence = &dead["reference_evidence"];
    assert_eq!(evidence["references"]["total"], 0);
    assert_eq!(evidence["scope"]["complete"], false);
    assert_eq!(evidence["classification"], "unverified");
    assert!(
        evidence["scope"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|notice| notice["reason"] == "unanalyzable_source")
    );
    assert_eq!(evidence["external_entry"]["status"], "unproven");
}
