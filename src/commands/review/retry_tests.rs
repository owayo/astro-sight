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
        base: "HEAD",
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
