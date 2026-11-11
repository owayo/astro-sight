use std::process::{Command, Output};

use serde_json::Value;

use super::support::{TestRepo, cargo_bin};

const PRODUCER: &str = "export function cube(v: number): number { return v * v * v; }\n";
const CONSUMER: &str = "import { cube } from './math';\nconsole.log(cube(3));\n";

fn fixture() -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("math.ts", "export {};\n");
    repo.write("reader.ts", "export {};\n");
    repo.commit_all("base");
    repo.write("math.ts", PRODUCER);
    repo
}

fn producer_diff(repo: &TestRepo) -> String {
    let output = Command::new("git")
        .current_dir(repo.root())
        .args([
            "--no-pager",
            "-c",
            "diff.external=",
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "HEAD",
            "--",
            "math.ts",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn run(repo: &TestRepo, args: &[&str]) -> Output {
    cargo_bin()
        .current_dir(repo.root())
        .args(["review", "--dir", "."])
        .args(args)
        .output()
        .unwrap()
}

fn hook(repo: &TestRepo, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--hook");
    let output = run(repo, &args);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stderr).expect("hook JSON");
    assert_eq!(value["blocking_categories"], serde_json::json!([]));
    value
}

fn cube_summary(value: &Value, hook: bool) -> &Value {
    let items = if hook {
        &value["added_reference_summary"]
    } else {
        &value["api_changes"]["added_reference_summary"]
    };
    items
        .as_array()
        .expect("added reference summaries")
        .iter()
        .find(|item| item["file"] == "math.ts" && item["name"] == "cube")
        .expect("cube summary, including when api.add suppresses it")
}

fn counts(summary: &Value, inside: usize, outside: usize) {
    for (key, expected) in [("in_diff", inside), ("outside_diff", outside)] {
        assert_eq!(summary[key]["observed"], expected, "{summary}");
        assert_eq!(summary[key]["total"], expected, "{summary}");
        assert_eq!(summary[key]["complete"], true, "{summary}");
        assert_eq!(summary[key]["omitted"], 0, "{summary}");
    }
    assert_eq!(summary["scope"]["partition"], "diff_files");
    assert_eq!(summary["scope"]["attribution"], "name_match");
    assert_eq!(
        summary["scope"]["references"],
        "non_definition_identifier_occurrences"
    );
}

#[test]
fn added_reference_summary_keeps_add_policy_and_partitions_same_workspace() {
    let repo = fixture();
    repo.write("reader.ts", CONSUMER);
    let diff = producer_diff(&repo);
    repo.write("producer.diff", diff);
    let producer_only = hook(&repo, &["--diff-file", "producer.diff"]);
    counts(cube_summary(&producer_only, true), 0, 2);
    assert_eq!(producer_only["api"]["add"][0]["n"], "cube");
    assert_eq!(
        producer_only["api"]["add_scope"],
        "no_cross_file_refs_in_diff"
    );

    let whole_diff = hook(&repo, &["--git"]);
    counts(cube_summary(&whole_diff, true), 2, 0);
    assert!(whole_diff["api"]["add"].is_null(), "{whole_diff}");
    assert!(whole_diff["api"]["add_scope"].is_null(), "{whole_diff}");
    let normal = repo.run_json("review", &["--git"]);
    counts(cube_summary(&normal, false), 2, 0);
    assert_eq!(normal["api_changes"]["added"], serde_json::json!([]));
}

#[test]
fn added_reference_summary_reports_zero_in_scope_without_changing_exit() {
    let repo = fixture();
    let value = hook(&repo, &["--git"]);
    counts(cube_summary(&value, true), 0, 0);
    assert_eq!(value["api"]["add"][0]["n"], "cube");
    assert_eq!(
        cube_summary(&value, true)["scope"]["new_side"],
        "working_tree"
    );
}

#[test]
fn added_reference_summary_staged_ignores_unstaged_consumers() {
    let repo = fixture();
    repo.git(["add", "math.ts"]);
    repo.write("reader.ts", CONSUMER);
    let value = hook(&repo, &["--git", "--staged"]);
    counts(cube_summary(&value, true), 0, 0);
    assert_eq!(cube_summary(&value, true)["scope"]["new_side"], "index");
    assert_eq!(value["api"]["add"][0]["n"], "cube");
}

#[test]
fn added_reference_summary_staged_keeps_consumers_removed_from_worktree() {
    let repo = fixture();
    repo.write("reader.ts", CONSUMER);
    repo.stage_all();
    repo.write("reader.ts", "export {};\n");
    let value = hook(&repo, &["--git", "--staged"]);
    counts(cube_summary(&value, true), 2, 0);
    assert_eq!(cube_summary(&value, true)["scope"]["new_side"], "index");
    assert!(value["api"]["add"].is_null(), "{value}");
    let normal = repo.run_json("review", &["--git", "--staged"]);
    counts(cube_summary(&normal, false), 2, 0);
}

#[test]
fn added_reference_summary_staged_counts_unchanged_index_consumer_outside_diff() {
    let repo = fixture();
    repo.write("reader.ts", CONSUMER);
    repo.git(["add", "reader.ts"]);
    repo.git(["commit", "-qm", "consumer first"]);
    repo.git(["add", "math.ts"]);
    repo.write("reader.ts", "export {};\n");
    let value = hook(&repo, &["--git", "--staged"]);
    counts(cube_summary(&value, true), 0, 2);
    assert_eq!(value["api"]["add"][0]["n"], "cube");
}

#[test]
fn added_reference_summary_incomplete_source_never_claims_exact_zero() {
    let repo = fixture();
    repo.write("Widget.vue", "<template>{{ cube(3) }}</template>\n");
    let value = hook(&repo, &["--git"]);
    let summary = cube_summary(&value, true);
    for key in ["in_diff", "outside_diff"] {
        assert_eq!(summary[key]["observed"], 0);
        assert_eq!(summary[key]["complete"], false);
        assert!(summary[key]["total"].is_null());
    }
    assert!(
        summary["scan"]["truncations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|truncation| truncation["reason"] == "unanalyzable_source")
    );
    assert_eq!(value["api"]["add"][0]["n"], "cube");
}

#[test]
fn added_reference_summary_recovered_parse_is_incomplete_without_changing_refs() {
    let repo = fixture();
    repo.write("reader.ts", "cube(3);\nfunction broken( {\n");
    let normal = repo.run_json("review", &["--git"]);
    let summary = cube_summary(&normal, false);
    assert_eq!(summary["in_diff"]["observed"], 1, "{summary}");
    assert_eq!(summary["scan"]["parse_error_files"], 1, "{summary}");
    assert!(!summary["in_diff"]["complete"].as_bool().unwrap());
    assert!(summary["in_diff"]["total"].is_null());
    assert!(summary["outside_diff"]["total"].is_null());
    let references = repo.run_json("refs", &["--name", "cube"]);
    assert!(
        references["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reference| { reference["path"] == "reader.ts" && reference["ln"] == 0 })
    );
}

#[test]
fn added_reference_summary_generated_skip_is_explicit_and_incomplete() {
    let repo = fixture();
    repo.write(".gitattributes", "generated.ts linguist-generated\n");
    repo.write("generated.ts", CONSUMER);
    let value = hook(&repo, &["--git"]);
    let summary = cube_summary(&value, true);
    assert_eq!(summary["scan"]["generated_files_skipped"]["generated"], 1);
    assert_eq!(summary["in_diff"]["observed"], 0);
    assert_eq!(summary["in_diff"]["complete"], false);
    assert!(summary["in_diff"]["total"].is_null());
    assert!(summary["outside_diff"]["total"].is_null());
    assert_eq!(value["api"]["add"][0]["n"], "cube");
}

#[test]
fn added_reference_summary_same_name_occurrences_do_not_claim_owner_resolution() {
    let repo = fixture();
    repo.write(
        "unrelated.ts",
        "function cube(v: number) { return v; }\ncube(1);\n",
    );
    repo.git(["add", "unrelated.ts"]);
    repo.git(["commit", "-qm", "other cube"]);
    let value = hook(&repo, &["--git"]);
    let summary = cube_summary(&value, true);
    counts(summary, 0, 1);
    assert_eq!(
        summary["outside_diff"]["references"][0]["path"],
        "unrelated.ts"
    );
    assert_eq!(summary["scope"]["attribution"], "name_match");
    assert_eq!(value["api"]["add"][0]["n"], "cube");
}

#[test]
fn added_reference_summary_failed_source_is_not_an_exact_zero() {
    let repo = fixture();
    std::fs::File::create(repo.path("oversized.ts"))
        .unwrap()
        .set_len(101 * 1024 * 1024)
        .unwrap();
    let normal = repo.run_json("review", &["--git"]);
    let summary = cube_summary(&normal, false);
    assert_eq!(summary["scan"]["failed_files"], 1);
    assert_eq!(summary["in_diff"]["observed"], 0);
    assert!(summary["in_diff"]["total"].is_null());
    assert!(summary["outside_diff"]["total"].is_null());
}

#[test]
fn added_reference_summary_location_samples_report_omissions_deterministically() {
    let repo = fixture();
    repo.write(
        "reader.ts",
        format!(
            "import {{ cube }} from './math';\n{}",
            "cube(3);\n".repeat(30)
        ),
    );
    let value = hook(&repo, &["--git"]);
    let summary = cube_summary(&value, true);
    assert_eq!(summary["in_diff"]["observed"], 31);
    assert_eq!(summary["in_diff"]["total"], 31);
    assert_eq!(summary["in_diff"]["omitted"], 11);
    assert_eq!(
        summary["in_diff"]["references"].as_array().unwrap().len(),
        20
    );
    assert_eq!(summary["in_diff"]["references"][0]["path"], "reader.ts");
    assert_eq!(summary["in_diff"]["references"][0]["line"], 0);
    assert_eq!(summary["in_diff"]["references"][19]["line"], 19);
    let repeated = hook(&repo, &["--git"]);
    assert_eq!(summary, cube_summary(&repeated, true));
}

#[test]
fn added_reference_summary_toon_preserves_additive_evidence() {
    let repo = fixture();
    repo.write("reader.ts", CONSUMER);
    let output = run(&repo, &["--git", "--format", "toon"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("added_reference_summary"), "{text}");
    assert!(text.contains("name_match"), "{text}");
    assert!(text.contains("total: 2"), "{text}");
    assert!(
        text.contains("non_definition_identifier_occurrences"),
        "{text}"
    );
}

#[test]
fn added_reference_summary_empty_diff_keeps_hook_silent() {
    let repo = fixture();
    repo.write("math.ts", "export {};\n");
    let output = run(&repo, &["--git", "--hook"]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn added_reference_summary_preserves_modified_api_blocking() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("math.ts", PRODUCER);
    repo.write("reader.ts", CONSUMER);
    repo.commit_all("base");
    repo.write(
        "math.ts",
        "export function cube(v: number, scale: number): number { return v * scale; }\n",
    );
    let output = run(&repo, &["--git", "--hook"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(value["added_reference_summary"].is_null(), "{value}");
    assert_eq!(value["api"]["mod"][0]["n"], "cube");
    assert!(
        value["blocking_categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "api.mod")
    );
}

#[test]
fn added_reference_summary_includes_rust_reexport_suppressed_by_diff_consumer() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"summary\"\nversion = \"0.1.0\"\n[lib]\npath = \"lib.rs\"\n",
    );
    repo.write("lib.rs", "mod math;\nmod reader;\n");
    repo.write("math.rs", "pub fn cube(v: i32) -> i32 { v * v * v }\n");
    repo.write("reader.rs", "pub fn reader() -> i32 { 0 }\n");
    repo.commit_all("base");
    repo.write("lib.rs", "mod math;\nmod reader;\npub use math::cube;\n");
    repo.write("reader.rs", "pub fn reader() -> i32 { crate::cube(3) }\n");
    let normal = repo.run_json("review", &["--git"]);
    let summaries = normal["api_changes"]["added_reference_summary"]
        .as_array()
        .unwrap();
    let summary = summaries
        .iter()
        .find(|summary| summary["file"] == "lib.rs" && summary["name"] == "cube")
        .expect("new reexport remains visible");
    counts(summary, 2, 0);
    assert!(
        normal["api_changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["name"] != "cube")
    );
}
