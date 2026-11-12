use super::support::*;

fn host_entry_vscode_repo(config: bool) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write(
        "package.json",
        r#"{"name":"demo","engines":{"vscode":"^1.90.0"},"main":"./out/extension.js"}"#,
    );
    if config {
        repo.write(
            "tsconfig.json",
            r#"{"compilerOptions":{"rootDir":"src","outDir":"out"}}"#,
        );
    }
    repo.write("src/extension.ts", "export function activate(context: unknown) {}\nexport function deactivate() {}\nexport function unused() {}\n");
    repo.commit_all("baseline");
    repo
}

#[test]
fn host_entry_vscode_proof_is_reported_in_dead_review_and_hook() {
    let repo = host_entry_vscode_repo(true);
    let dead = repo.run_json("dead-code", &[]);
    assert_eq!(dead["dead_symbols"].as_array().unwrap().len(), 1, "{dead}");
    assert_eq!(dead["dead_symbols"][0]["name"], "unused");
    let entries = dead["external_entry_symbols"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0]["reference_evidence"]["external_entry"]["status"],
        "proven"
    );
    assert!(
        entries[0]["reference_evidence"]["external_entry"]["registrations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["file"] == "package.json" && r["script"] == "main")
    );
    repo.write("src/extension.ts", "export async function activate(context: unknown) { await Promise.resolve(); }\nexport function deactivate() {}\nexport function unused() {}\n");
    let review = repo.run_json("review", &["--git"]);
    assert!(
        !review["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
    assert!(
        review["external_entry_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
    let hook = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let hook: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
    assert!(
        hook["external_entry"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["n"] == "activate")
    );
}

#[test]
fn host_entry_vscode_without_source_mapping_is_unverified() {
    let repo = host_entry_vscode_repo(false);
    let result = repo.run_json("dead-code", &[]);
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
    let candidate = result["unverified_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "activate")
        .unwrap();
    assert_eq!(
        candidate["reference_evidence"]["external_entry"]["status"],
        "unproven"
    );
    repo.write("src/extension.ts", "export async function activate(context: unknown) {}\nexport function deactivate() {}\nexport function unused() {}\n");
    let review = repo.run_json("review", &["--git"]);
    assert!(
        review["unverified_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
    let hook = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let hook: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
    assert!(
        hook["unverified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["n"] == "activate")
    );
}

#[test]
fn host_entry_vscode_requires_manifest_target_and_host_declaration() {
    let repo = host_entry_vscode_repo(true);
    repo.write("src/other.ts", "export function activate() {}\n");
    let result = repo.run_json("dead-code", &[]);
    assert!(
        result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate" && s["file"] == "src/other.ts")
    );
    repo.remove_file("src/other.ts");
    for manifest in [
        r#"{"main":"./out/extension.js"}"#,
        r#"{"engines":{"vscode":"*"},"main":"./out/missing.js"}"#,
    ] {
        repo.write("package.json", manifest);
        let result = repo.run_json("dead-code", &[]);
        assert!(
            result["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "activate" && s["file"] == "src/extension.ts"),
            "{result}"
        );
    }
    repo.remove_file("package.json");
    let result = repo.run_json("dead-code", &[]);
    assert!(
        result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate" && s["file"] == "src/extension.ts")
    );
}

#[test]
fn host_entry_explicit_declaration_is_exact_and_stale_targets_fail() {
    let repo = TestRepo::new();
    repo.create_dir_all(".astro-sight");
    repo.write(
        "hook.ts",
        "export function afterEdit(value: unknown) {}\nexport function unused() {}\n",
    );
    repo.write("other.ts", "export function afterEdit() {}\n");
    repo.write(".astro-sight/entrypoints.json", r#"{"entries":[{"file":"hook.ts","export":"afterEdit","reason":"Editor host invokes this export"}]}"#);
    let result = repo.run_json("dead-code", &[]);
    assert!(
        result["external_entry_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "afterEdit" && s["file"] == "hook.ts")
    );
    assert!(
        result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "afterEdit" && s["file"] == "other.ts")
    );
    for declaration in [
        r#"{"entries":[{"file":"missing.ts","export":"afterEdit","reason":"Host"}]}"#,
        r#"{"entries":[{"file":"hook.ts","export":"missing","reason":"Host"}]}"#,
        r#"{"entries":[{"file":"../outside.ts","export":"afterEdit","reason":"Host"}]}"#,
        r#"{"entries":[{"file":"hook.ts","export":"afterEdit","reason":""}]}"#,
    ] {
        repo.write(".astro-sight/entrypoints.json", declaration);
        let output = cargo_bin()
            .args(["dead-code", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "INVALID_REQUEST", "{error}");
    }
}

#[test]
fn host_entry_vscode_direct_main_browser_and_nested_packages() {
    let repo = TestRepo::new();
    repo.create_dir_all("nested");
    repo.write(
        "package.json",
        r#"{"engines":{"vscode":"*"},"main":"./desktop.js","browser":"./web.js"}"#,
    );
    repo.write("desktop.js", "export function activate() {}\n");
    repo.write("web.js", "export const deactivate = () => {};\n");
    repo.write("nested/package.json", r#"{"name":"unrelated"}"#);
    repo.write("nested/desktop.js", "export function activate() {}\n");
    let result = repo.run_json("dead-code", &[]);
    let entries = result["external_entry_symbols"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|s| s["name"] == "activate" && s["file"] == "desktop.js")
    );
    assert!(
        entries
            .iter()
            .any(|s| s["name"] == "deactivate" && s["file"] == "web.js")
    );
    assert!(
        result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["file"] == "nested/desktop.js" && s["name"] == "activate")
    );
}

#[test]
fn host_entry_staged_manifest_and_registry_use_the_index() {
    let repo = host_entry_vscode_repo(true);
    repo.write("src/extension.ts", "export async function activate(context: unknown) {}\nexport function deactivate() {}\nexport function unused() {}\n");
    repo.stage_all();
    repo.write(
        "package.json",
        r#"{"engines":{"vscode":"*"},"main":"./out/missing.js"}"#,
    );
    let staged = repo.run_json("review", &["--git", "--staged"]);
    assert!(
        staged["external_entry_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
    let working = repo.run_json("review", &["--git"]);
    assert!(
        working["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
    let staged_dead = repo.run_json("dead-code", &["--git", "--staged"]);
    assert!(
        staged_dead["external_entry_symbols"]
            .as_array()
            .is_some_and(|symbols| symbols.iter().any(|s| s["name"] == "activate")),
        "{staged_dead}"
    );

    let explicit = TestRepo::new();
    explicit.init_git();
    explicit.create_dir_all(".astro-sight");
    explicit.write("entry.ts", "export function onChange() {}\n");
    explicit.write(
        ".astro-sight/entrypoints.json",
        r#"{"entries":[{"file":"entry.ts","export":"onChange","reason":"Editor host"}]}"#,
    );
    explicit.commit_all("baseline");
    explicit.write("entry.ts", "export async function onChange() {}\n");
    explicit.stage_all();
    explicit.write(".astro-sight/entrypoints.json", r#"{"entries":[]}"#);
    let staged = explicit.run_json("review", &["--git", "--staged"]);
    assert_eq!(staged["external_entry_symbols"][0]["name"], "onChange");
    let staged_dead = explicit.run_json("dead-code", &["--git", "--staged"]);
    assert_eq!(staged_dead["external_entry_symbols"][0]["name"], "onChange");
    let working = explicit.run_json("review", &["--git"]);
    assert_eq!(working["dead_symbols"][0]["name"], "onChange");
}

#[test]
fn host_entry_staged_snapshot_failure_does_not_reuse_worktree_proof() {
    let repo = host_entry_vscode_repo(true);
    repo.write(
        "src/extension.ts",
        "export async function activate(context: unknown) {}\n",
    );
    repo.stage_all();
    repo.write("blocked-temp", "snapshot creation must fail\n");
    let output = cargo_bin()
        .env("TEMP", repo.path("blocked-temp"))
        .env("TMP", repo.path("blocked-temp"))
        .env("TMPDIR", repo.path("blocked-temp"))
        .args(["dead-code", "--dir"])
        .arg(repo.root())
        .args(["--git", "--staged"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(error["error"]["code"].is_string(), "{error}");
    assert!(error["external_entry_symbols"].is_null());
}

#[test]
fn host_entry_ambiguous_sources_and_rebound_exports_are_unverified() {
    let repo = host_entry_vscode_repo(true);
    repo.write("src/extension.tsx", "export function activate() {}\n");
    let result = repo.run_json("dead-code", &[]);
    assert!(result["external_entry_symbols"].is_null());
    assert!(
        result["unverified_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
    repo.remove_file("src/extension.tsx");
    repo.write(
        "src/extension.ts",
        "export let activate = () => {};\nactivate = () => {};\n",
    );
    let result = repo.run_json("dead-code", &[]);
    assert!(result["external_entry_symbols"].is_null());
    assert!(
        !result["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
}

#[test]
fn host_entry_explicit_declarations_reject_default_exports_and_rebindings() {
    let repo = TestRepo::new();
    repo.create_dir_all(".astro-sight");
    repo.write(
        ".astro-sight/entrypoints.json",
        r#"{"entries":[{"file":"entry.ts","export":"activate","reason":"Host"}]}"#,
    );
    for source in [
        "export default function activate() {}",
        "export let activate = () => {}; activate = () => {};",
    ] {
        repo.write("entry.ts", source);
        let output = cargo_bin()
            .args(["dead-code", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "INVALID_REQUEST");
    }
}

#[test]
fn host_entry_typescript_jsx_output_and_option_types_are_checked() {
    let repo = host_entry_vscode_repo(true);
    repo.remove_file("src/extension.ts");
    repo.write("src/extension.tsx", "export function activate() {}\n");
    repo.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"rootDir":"src","outDir":"out","jsx":"preserve"}}"#,
    );
    let wrong_output = repo.run_json("dead-code", &[]);
    assert!(wrong_output["external_entry_symbols"].is_null());
    repo.write(
        "package.json",
        r#"{"engines":{"vscode":"*"},"main":"./out/extension.jsx"}"#,
    );
    let correct_output = repo.run_json("dead-code", &[]);
    assert_eq!(
        correct_output["external_entry_symbols"][0]["name"],
        "activate"
    );
    repo.write(
        "package.json",
        r#"{"engines":{"vscode":"*"},"main":"./out/extension.js"}"#,
    );
    repo.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"rootDir":"src","outDir":"out","jsx":"react-jsx"}}"#,
    );
    let transformed = repo.run_json("dead-code", &[]);
    assert_eq!(transformed["external_entry_symbols"][0]["name"], "activate");
    for field in ["noEmit", "emitDeclarationOnly", "allowJs", "rootDir", "jsx"] {
        let mut options = serde_json::json!({"rootDir":"src","outDir":"out"});
        options[field] = serde_json::json!(123);
        repo.write(
            "tsconfig.json",
            serde_json::json!({"compilerOptions":options}).to_string(),
        );
        let output = cargo_bin()
            .args(["dead-code", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{field}");
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "INVALID_REQUEST");
    }
}

#[test]
fn host_entry_export_bindings_handle_aliases_and_conflicts_in_both_orders() {
    let repo = TestRepo::new();
    repo.create_dir_all(".astro-sight");
    repo.write(
        ".astro-sight/entrypoints.json",
        r#"{"entries":[{"file":"entry.ts","export":"activate","reason":"Host"}]}"#,
    );
    repo.write("other.ts", "export function activate() {}\n");
    for source in [
        "import {activate} from './other'; export function activate() {}",
        "export function activate() {} import {activate} from './other';",
        "class activate {} export function activate() {}",
        "export function activate() {} class activate {}",
        "const {activate} = {activate: () => {}}; export function activate() {}",
        "const [activate] = []; export function activate() {}",
        "export let activate = () => {}; ({activate} = {activate: () => {}});",
    ] {
        repo.write("entry.ts", source);
        let output = cargo_bin()
            .args(["dead-code", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{source}");
    }
    repo.write(
        "entry.ts",
        "import {activate as fallback} from './other'; export function activate() {}\n",
    );
    let result = repo.run_json("dead-code", &[]);
    assert_eq!(result["external_entry_symbols"][0]["name"], "activate");
}

#[test]
fn host_entry_typescript_default_output_exclusion_and_explicit_files() {
    let repo = host_entry_vscode_repo(true);
    repo.write(
        "package.json",
        r#"{"engines":{"vscode":"*"},"main":"./src/extension.js"}"#,
    );
    repo.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"rootDir":"src","outDir":"src"}}"#,
    );
    let excluded = repo.run_json("dead-code", &[]);
    assert!(excluded["external_entry_symbols"].is_null(), "{excluded}");
    repo.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"rootDir":"src","outDir":"src"},"files":["src/extension.ts"]}"#,
    );
    let explicit = repo.run_json("dead-code", &[]);
    assert!(
        explicit["external_entry_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "activate")
    );
}

#[test]
fn host_entry_registration_does_not_close_a_breaking_api_change() {
    let repo = host_entry_vscode_repo(true);
    repo.write("src/extension.ts", "export function activate(context: number) {}\nexport function deactivate() {}\nexport function unused() {}\n");
    let result = repo.run_json("review", &["--git"]);
    assert!(
        result["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["name"] == "activate"),
        "{result}"
    );
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        hook["blocking_categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category == "api.mod"),
        "{hook}"
    );
    assert!(
        !hook["blocking_categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category == "dead")
    );
}

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
