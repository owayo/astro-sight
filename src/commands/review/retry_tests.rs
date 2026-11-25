use super::*;
use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
}

fn source(version: usize) -> String {
    format!(
        "export function target(first: number, v{version}: number): number {{ return first; }}\n"
    )
}

fn repository(staged: bool) -> tempfile::TempDir {
    let temporary = tempfile::tempdir().unwrap();
    let repo = temporary.path();
    crate::commands::tests::common::init_git_repo_for_test(repo);
    std::fs::write(
        repo.join("api.ts"),
        "export function target(): number { return 1; }\n",
    )
    .unwrap();
    git(repo, &["add", "api.ts"]);
    git(repo, &["commit", "-qm", "base"]);
    std::fs::write(repo.join("api.ts"), source(0)).unwrap();
    if staged {
        git(repo, &["add", "api.ts"]);
    }
    temporary
}

fn options(dir: &str, staged: bool) -> CmdReviewOpts<'_> {
    CmdReviewOpts {
        dir,
        diff: None,
        diff_file: None,
        git: true,
        base: None,
        staged,
        min_confidence: 0.5,
        cochange_min_samples: 0,
        output: OutputOptions::default(),
        hook: false,
        framework: None,
        extra_exclude_dirs: &[],
        extra_exclude_globs: &[],
        dead_scope: crate::cli::DeadScope::All,
        strict_public_const_values: false,
        strict_public_type_annotations: false,
        include_wip_dead: false,
        include_generated: false,
    }
}

#[test]
fn review_input_retry_reanalyzes_after_two_post_analysis_changes_and_drops_snapshots() {
    for (staged, change_index) in [(false, false), (true, true), (true, false)] {
        let temporary = repository(staged);
        let repo = temporary.path();
        let opts = options(repo.to_str().unwrap(), staged);
        let service = AppService::new();
        let mut snapshots: Vec<std::path::PathBuf> = Vec::new();
        let mut signatures = Vec::new();
        retry_review(true, |attempt| {
            assert!(
                snapshots.iter().all(|path| !path.exists()),
                "previous snapshot survived"
            );
            review_attempt(&service, &opts, |result, analysis_dir| {
                if staged {
                    assert_ne!(Path::new(analysis_dir), repo);
                    snapshots.push(Path::new(analysis_dir).to_path_buf());
                }
                let target = result
                    .api_changes
                    .modified
                    .iter()
                    .find(|item| item.name == "target")
                    .unwrap();
                signatures.push(target.new_signature.clone().expect("new signature"));
                if attempt <= 2 {
                    std::fs::write(repo.join("api.ts"), source(attempt)).unwrap();
                    if change_index {
                        git(repo, &["add", "api.ts"]);
                    }
                }
            })
        })
        .unwrap();
        assert_eq!(signatures.len(), 3);
        for (attempt, signature) in signatures.iter().enumerate() {
            let version = if staged && !change_index { 0 } else { attempt };
            assert!(
                signature.contains(&format!("v{version}:")),
                "{signatures:?}"
            );
        }
        assert!(
            snapshots.iter().all(|path| !path.exists()),
            "final snapshot survived"
        );
    }
}

#[test]
fn review_input_retry_exhaustion_after_analysis_drops_every_snapshot() {
    let temporary = repository(true);
    let repo = temporary.path();
    let opts = options(repo.to_str().unwrap(), true);
    let mut snapshots: Vec<std::path::PathBuf> = Vec::new();
    let mut count = 0;
    let error = retry_review(true, |attempt| {
        count += 1;
        assert!(snapshots.iter().all(|path| !path.exists()));
        review_attempt(&AppService::new(), &opts, |_, analysis_dir| {
            snapshots.push(Path::new(analysis_dir).to_path_buf());
            std::fs::write(repo.join("api.ts"), source(attempt)).unwrap();
            git(repo, &["add", "api.ts"]);
        })
    })
    .unwrap_err();
    assert!(review_input_changed(&error));
    assert_eq!(count, 3);
    assert!(snapshots.iter().all(|path| !path.exists()));
}

#[test]
fn review_input_retry_snapshot_diff_mismatch_is_transient_but_other_failures_are_not() {
    let temporary = repository(true);
    let repo = temporary.path();
    let dir = repo.to_str().unwrap();
    let base = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let base = base.trim();
    let diff = run_git_diff(dir, base, true).unwrap();
    std::fs::write(repo.join("api.ts"), source(1)).unwrap();
    git(repo, &["add", "api.ts"]);
    let result = prepare_staged_source(dir, base, &diff, index_snapshot::SnapshotBudget::DEFAULT);
    let error = result.err().expect("snapshot must reject an outdated diff");
    assert!(review_input_changed(&error), "{error:#}");
    let error = prepare_staged_source(
        dir,
        "missing-base",
        &diff,
        index_snapshot::SnapshotBudget::DEFAULT,
    )
    .err()
    .expect("invalid snapshot base");
    assert!(!review_input_changed(&error));
    assert_eq!(
        crate::commands::common::classify_error(&error).0,
        "INVALID_REQUEST"
    );
}

#[test]
fn review_input_retry_does_not_repeat_explicit_input_or_unrelated_errors() {
    for git_input in [false, true] {
        let mut count = 0;
        let error = retry_review::<()>(git_input, |_| {
            count += 1;
            Err(AstroError::new(
                if git_input {
                    ErrorCode::InvalidRequest
                } else {
                    ErrorCode::ReviewInputChanged
                },
                "test error",
            )
            .into())
        })
        .unwrap_err();
        assert_eq!(count, 1);
        assert_eq!(
            error.downcast_ref::<AstroError>().unwrap().message,
            "test error"
        );
    }
}

#[test]
fn review_swift_protocol_configuration_survives_a_retry() {
    let temporary = tempfile::tempdir().unwrap();
    let repo = temporary.path();
    crate::commands::tests::common::init_git_repo_for_test(repo);
    std::fs::write(
        repo.join("Widget.swift"),
        "import Acme\npublic struct Widget: Acme.P {}\n",
    )
    .unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let source = |number| {
        format!(
            "import Acme\npublic struct Widget: Acme.P {{\n public func refresh(_ value: Swift.String) {{ print({number}) }}\n public func helper() {{}}\n}}\n"
        )
    };
    std::fs::write(repo.join("Widget.swift"), source(0)).unwrap();
    let config: crate::config::Config = toml::from_str("[[swift_protocol_methods]]\nprotocol='Acme.P'\nmethod='refresh'\nlabels=['_']\nparameter_types=['Swift.String']\nreturn_type='Swift.Void'\n").unwrap();
    let opts = options(repo.to_str().unwrap(), false);
    let mut attempts = 0;
    retry_review(true, |attempt| {
        attempts += 1;
        review_attempt_with_swift_protocols(
            &AppService::new(),
            &opts,
            &config.swift_protocol_methods,
            index_snapshot::SnapshotBudget::DEFAULT,
            |result, _| {
                assert!(
                    !result
                        .dead_symbols
                        .iter()
                        .chain(&result.test_only_symbols)
                        .any(|symbol| symbol.name == "Widget.refresh")
                );
                assert!(
                    result
                        .dead_symbols
                        .iter()
                        .any(|symbol| symbol.name == "Widget.helper")
                );
                if attempt == 1 {
                    std::fs::write(repo.join("Widget.swift"), source(attempt)).unwrap();
                }
            },
        )
    })
    .unwrap();
    assert_eq!(attempts, 2);
}

#[test]
fn over_budget_review_compares_staged_api_with_the_requested_base() {
    let temporary = repository(false);
    let repo = temporary.path();
    let original = "export function removed() {}\nexport function changed(a: number) {}\nexport function kept() {}\n";
    std::fs::write(repo.join("api.ts"), original).unwrap();
    std::fs::write(repo.join("cold.txt"), "not code").unwrap();
    std::fs::write(
        repo.join("caller.ts"),
        "import {removed, kept} from './api';\nremoved(); kept();\n",
    )
    .unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "selected base"]);
    std::fs::write(
        repo.join("api.ts"),
        format!("{original}export function headOnly() {{}}\n"),
    )
    .unwrap();
    git(repo, &["add", "api.ts"]);
    git(repo, &["commit", "-qm", "later HEAD"]);
    std::fs::write(
        repo.join("api.ts"),
        "export function changed(a: number, staged: string) {}\nexport function kept() {}\n",
    )
    .unwrap();
    std::fs::write(repo.join("new.ts"), "export function addedInIndex() {}\n").unwrap();
    git(repo, &["add", "."]);
    std::fs::write(
        repo.join("api.ts"),
        "export function removed() {}\nexport function changed(a: number, unstaged: boolean) {}\n",
    )
    .unwrap();
    std::fs::remove_file(repo.join("new.ts")).unwrap();

    let mut opts = options(repo.to_str().unwrap(), true);
    opts.base = Some("HEAD~1");
    for budget in [
        index_snapshot::SnapshotBudget::DEFAULT,
        index_snapshot::SnapshotBudget {
            files: 3,
            bytes: u64::MAX,
        },
    ] {
        review_attempt_with_swift_protocols(
            &AppService::new(),
            &opts,
            &SwiftProtocolMethods::default(),
            budget,
            |review, _| {
                let api = &review.api_changes;
                assert_eq!(
                    api.removed
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>(),
                    ["removed"],
                    "{api:?}"
                );
                assert!(
                    api.added.iter().any(|s| s.name == "addedInIndex"),
                    "{api:?}"
                );
                let changed = api
                    .modified
                    .iter()
                    .find(|s| s.name == "changed")
                    .expect("staged signature");
                assert!(
                    changed
                        .new_signature
                        .as_deref()
                        .unwrap()
                        .contains("staged: string"),
                    "{changed:?}"
                );
                assert_eq!(
                    review.truncations.iter().any(|t| t.reason
                        == crate::models::truncation::TruncationReason::StagedSnapshotUnavailable),
                    budget.files == 3
                );
            },
        )
        .unwrap();
    }
}

#[test]
fn over_budget_staged_api_uses_index_attributes_and_limits_its_own_inputs() {
    let temporary = repository(false);
    let repo = temporary.path();
    std::fs::write(repo.join(".gitattributes"), "api.ts -linguist-generated\n").unwrap();
    std::fs::write(repo.join("cold.dat"), "padding").unwrap();
    git(repo, &["add", ".gitattributes", "cold.dat"]);
    git(repo, &["commit", "-qm", "index attributes"]);
    std::fs::write(repo.join("api.ts"), "export const remaining = 1;\n").unwrap();
    git(repo, &["add", "api.ts"]);
    let opts = options(repo.to_str().unwrap(), true);
    let budget = index_snapshot::SnapshotBudget {
        files: 2,
        bytes: u64::MAX,
    };
    for generated in [false, true] {
        let attr = |flag| {
            if flag {
                "api.ts linguist-generated\n"
            } else {
                "api.ts -linguist-generated\n"
            }
        };
        std::fs::write(repo.join(".gitattributes"), attr(generated)).unwrap();
        git(repo, &["add", ".gitattributes"]);
        std::fs::write(repo.join(".gitattributes"), attr(!generated)).unwrap();
        review_attempt_with_swift_protocols(
            &AppService::new(),
            &opts,
            &SwiftProtocolMethods::default(),
            budget,
            |review, _| {
                assert_eq!(
                    review
                        .api_changes
                        .removed
                        .iter()
                        .any(|item| item.name == "target"),
                    !generated
                );
            },
        )
        .unwrap();
    }
    let error = review_attempt_with_swift_protocols(
        &AppService::new(),
        &opts,
        &SwiftProtocolMethods::default(),
        index_snapshot::SnapshotBudget { files: 1, ..budget },
        |_, _| panic!("partial output"),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("Staged API sources exceed"),
        "{error:#}"
    );
}

#[test]
fn over_budget_staged_strip_internal_reads_the_index_extends_target() {
    let temporary = repository(false);
    let repo = temporary.path();
    std::fs::create_dir_all(repo.join("node_modules/publisher")).unwrap();
    std::fs::write(
        repo.join("tsconfig.json"),
        r#"{"compilerOptions":{"stripInternal":false},"include":["*.ts"]}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("node_modules/publisher/publish.settings"),
        r#"{"compilerOptions":{"stripInternal":true}}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("caller.ts"),
        "import { internalOnly } from './api'; internalOnly();\n",
    )
    .unwrap();
    std::fs::write(repo.join("cold.dat"), "padding").unwrap();
    std::fs::write(
        repo.join("api.ts"),
        "/** @internal */\nexport function internalOnly() {}\nexport function publicName() {}\n",
    )
    .unwrap();
    git(repo, &["add", "."]);
    git(
        repo,
        &["add", "-f", "node_modules/publisher/publish.settings"],
    );
    git(repo, &["commit", "-qm", "internal declarations"]);
    std::fs::write(
        repo.join("tsconfig.json"),
        r#"{"extends":"./node_modules/publisher/publish.settings","include":["*.ts"]}"#,
    )
    .unwrap();
    git(repo, &["add", "tsconfig.json"]);
    std::fs::write(
        repo.join("node_modules/publisher/publish.settings"),
        r#"{"compilerOptions":{"stripInternal":false}}"#,
    )
    .unwrap();
    let opts = options(repo.to_str().unwrap(), true);
    for budget in [
        index_snapshot::SnapshotBudget::DEFAULT,
        index_snapshot::SnapshotBudget {
            files: 4,
            bytes: u64::MAX,
        },
    ] {
        review_attempt_with_swift_protocols(
            &AppService::new(),
            &opts,
            &SwiftProtocolMethods::default(),
            budget,
            |review, _| {
                assert!(
                    review
                        .api_changes
                        .removed
                        .iter()
                        .any(|symbol| symbol.name == "internalOnly"),
                    "{:?}",
                    review.api_changes
                );
            },
        )
        .unwrap();
    }
}
