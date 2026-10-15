//! Git の入力競合を子プロセス内で同期的に起こし、review の試行と出力を検証する。
#![cfg(unix)]

use super::support::{TestRepo, cargo_bin};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

const BASE: &str = "export function target(): number { return 1; }\n";
const FIRST: &str = "export function target(first: number): number { return first; }\n";
const LATEST: &str =
    "export function target(first: number, latest: number): number { return first + latest; }\n";

struct GitRace {
    temporary: tempfile::TempDir,
    path: std::ffi::OsString,
    real_git: std::path::PathBuf,
}

impl GitRace {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let wrapper = temporary.path().join("git");
        std::fs::write(
            &wrapper,
            r#"#!/bin/sh
attempt=0
if [ -f "$ASTRO_RETRY_STATE/attempts" ]; then read -r attempt < "$ASTRO_RETRY_STATE/attempts"; fi
is_diff=false
is_snapshot_index=false
is_input_diff=false
for arg in "$@"; do
  if [ "$arg" = 'HEAD^{commit}' ] || [ "$arg" = 'missing-base^{commit}' ]; then
    attempt=$((attempt + 1))
    printf '%s\n' "$attempt" > "$ASTRO_RETRY_STATE/attempts"
  fi
  if [ "$arg" = diff ]; then is_diff=true; fi
  if [ "$arg" = index ]; then is_snapshot_index=true; fi
  if [ "$arg" = --find-renames ]; then is_input_diff=true; fi
  case "$arg" in
    */astro-sight-index-*) printf '%s\n' "$arg" >> "$ASTRO_RETRY_STATE/snapshots" ;;
  esac
done
if $is_diff && [ "$PWD" = "$ASTRO_RETRY_REPO" ]; then
  if $is_input_diff; then printf '%s\n' "$LC_ALL" >> "$ASTRO_RETRY_STATE/locales"; fi
  if [ "$ASTRO_RETRY_MODE" = other ]; then
    printf 'fatal: permission denied\n' >&2
    exit 128
  fi
  if [ "$ASTRO_RETRY_MODE" = short ] && [ "$attempt" -le "$ASTRO_RETRY_FAILURES" ]; then
    printf 'error: short read while indexing api.ts\n' >&2
    exit 128
  fi
  if [ "$ASTRO_RETRY_MODE" = lookalike ]; then
    printf 'fatal: cannot open error: short read while indexing api.ts\n' >&2
    exit 128
  fi
fi
"$ASTRO_RETRY_REAL_GIT" "$@"
result=$?
if [ "$result" -eq 0 ] && $is_snapshot_index && [ "$PWD" = "$ASTRO_RETRY_REPO" ] && [ "$ASTRO_RETRY_MODE" = snapshot ] && [ "$attempt" -eq 1 ]; then
  cp "$ASTRO_RETRY_STATE/latest.ts" api.ts || exit 1
  "$ASTRO_RETRY_REAL_GIT" add api.ts || exit 1
fi
if [ "$result" -eq 0 ] && $is_diff && [ "$PWD" = "$ASTRO_RETRY_REPO" ]; then
  last=0
  if [ -f "$ASTRO_RETRY_STATE/changed" ]; then read -r last < "$ASTRO_RETRY_STATE/changed"; fi
  if [ "$last" != "$attempt" ]; then
    case "$ASTRO_RETRY_MODE" in
      latest)
        if [ "$attempt" -eq 1 ]; then
          cp "$ASTRO_RETRY_STATE/latest.ts" api.ts || exit 1
          if [ "$ASTRO_RETRY_STAGED" = true ]; then "$ASTRO_RETRY_REAL_GIT" add api.ts || exit 1; fi
        fi
        ;;
      changing) printf '%s\n' "$attempt" >> note.md ;;
    esac
    printf '%s\n' "$attempt" > "$ASTRO_RETRY_STATE/changed"
  fi
fi
exit "$result"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions).unwrap();
        let host_path = std::env::var_os("PATH").unwrap();
        let real_git = std::env::split_paths(&host_path)
            .map(|path| path.join("git"))
            .find(|path| path.is_file())
            .unwrap();
        let mut paths = vec![temporary.path().to_path_buf()];
        paths.extend(std::env::split_paths(&host_path));
        let path = std::env::join_paths(paths).unwrap();
        std::fs::write(temporary.path().join("latest.ts"), LATEST).unwrap();
        Self {
            temporary,
            path,
            real_git,
        }
    }

    fn command(&self, repo: &TestRepo, mode: &str, staged: bool) -> Command {
        let mut command = self.base_command(repo, "review", mode, staged);
        command.arg("--git");
        command
    }

    fn base_command(&self, repo: &TestRepo, subcommand: &str, mode: &str, staged: bool) -> Command {
        let mut command = cargo_bin();
        command
            .args([subcommand, "--dir"])
            .arg(repo.root())
            .env("PATH", &self.path)
            .env("ASTRO_RETRY_REAL_GIT", &self.real_git)
            .env("ASTRO_RETRY_REPO", repo.root().canonicalize().unwrap())
            .env("ASTRO_RETRY_STATE", self.temporary.path())
            .env("ASTRO_RETRY_MODE", mode)
            .env("ASTRO_RETRY_FAILURES", "0")
            .env("ASTRO_RETRY_STAGED", staged.to_string());
        if staged {
            command.arg("--staged");
        }
        command
    }

    fn attempts(&self) -> usize {
        std::fs::read_to_string(self.temporary.path().join("attempts"))
            .unwrap_or_else(|_| "0".into())
            .trim()
            .parse()
            .unwrap()
    }
}

fn source_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("api.ts", BASE);
    repo.write(
        "caller.ts",
        "import { target } from './api';\nexport function caller() { return target(); }\n",
    );
    repo.write("note.md", "base\n");
    repo.init_git();
    repo.commit_all("base");
    repo.write("api.ts", FIRST);
    repo
}

fn one_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(bytes)))
}

fn assert_error(output: &Output, code: &str, exit: i32) {
    assert_eq!(output.status.code(), Some(exit), "{output:?}");
    assert_eq!(
        one_json(&output.stdout)["error"]["code"],
        code,
        "{output:?}"
    );
}

#[test]
fn review_input_retry_discards_the_first_diff_and_uses_the_latest_signature() {
    for (mode, staged) in [("latest", false), ("latest", true), ("snapshot", true)] {
        for hook in [false, true] {
            let repo = source_repo();
            if staged {
                repo.stage_all();
            }
            let race = GitRace::new();
            let mut command = race.command(&repo, mode, staged);
            if hook {
                command.arg("--hook");
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(i32::from(hook)), "{output:?}");
            assert_eq!(race.attempts(), 2, "{output:?}");
            if staged {
                let snapshots =
                    std::fs::read_to_string(race.temporary.path().join("snapshots")).unwrap();
                assert!(!snapshots.is_empty());
                assert!(
                    snapshots
                        .lines()
                        .all(|path| !std::path::Path::new(path).exists()),
                    "{snapshots}"
                );
            }
            if hook {
                assert!(output.stdout.is_empty(), "{output:?}");
                assert!(
                    one_json(&output.stderr)["api"]["mod"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["n"] == "target")
                );
            } else {
                let result = one_json(&output.stdout);
                let modified = result["api_changes"]["modified"].as_array().unwrap();
                assert!(
                    modified.iter().any(|item| item["name"] == "target"
                        && item["new_signature"].as_str().unwrap().contains("latest")),
                    "{result}"
                );
                assert!(output.stderr.is_empty(), "{output:?}");
            }
        }
    }
}

#[test]
fn review_input_retry_exhaustion_has_one_error_and_a_distinct_exit_code() {
    for hook in [false, true] {
        let repo = source_repo();
        let race = GitRace::new();
        let mut command = race.command(&repo, "changing", false);
        if hook {
            command.arg("--hook");
        }
        let output = command.output().unwrap();
        assert_error(&output, "REVIEW_INPUT_CHANGED", 2);
        assert_eq!(race.attempts(), 3);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if hook {
            assert!(stderr.contains("run review again"), "{stderr}");
            assert_eq!(stderr.lines().count(), 1, "{stderr}");
        } else {
            assert!(stderr.is_empty(), "{stderr}");
        }
    }
}

#[test]
fn review_input_retry_discards_blocking_findings_when_the_latest_contract_is_compatible() {
    let repo = source_repo();
    let race = GitRace::new();
    std::fs::write(
        race.temporary.path().join("latest.ts"),
        "export function target(first?: number): number { return first ?? 1; }\n",
    )
    .unwrap();
    let output = race
        .command(&repo, "latest", false)
        .arg("--hook")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(race.attempts(), 2);
    assert!(output.stdout.is_empty(), "{output:?}");
    let result = one_json(&output.stderr);
    assert!(
        result["api"]["mod_compat"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["n"] == "target" && item["reason"] == "trailing_optional_params" }),
        "{result}"
    );
    assert!(
        result["api"]["mod"].as_array().is_none_or(Vec::is_empty),
        "{result}"
    );
    assert!(
        result["impacts"].as_array().is_none_or(Vec::is_empty),
        "{result}"
    );
    assert!(
        result["blocking_categories"].as_array().unwrap().is_empty(),
        "{result}"
    );
}

#[test]
fn review_input_retry_preserves_other_git_errors() {
    for mode in ["other", "lookalike"] {
        let repo = source_repo();
        let race = GitRace::new();
        let output = race.command(&repo, mode, false).output().unwrap();
        assert_error(&output, "INVALID_REQUEST", 1);
        assert_eq!(race.attempts(), 1);
        assert!(
            one_json(&output.stdout)["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("git diff failed:")
        );
    }
}

#[test]
fn review_input_retry_short_reads_are_typed_and_limited_to_three_attempts() {
    for failures in [1, 2, 3] {
        for hook in [false, true] {
            let repo = source_repo();
            let race = GitRace::new();
            let mut command = race.command(&repo, "short", false);
            command.env("ASTRO_RETRY_FAILURES", failures.to_string());
            if hook {
                command.arg("--hook");
            }
            let output = command.output().unwrap();
            assert_eq!(race.attempts(), (failures + 1).min(3));
            if failures == 3 {
                assert_error(&output, "REVIEW_INPUT_CHANGED", 2);
            } else {
                assert_eq!(output.status.code(), Some(i32::from(hook)), "{output:?}");
                if hook {
                    one_json(&output.stderr);
                    assert!(output.stdout.is_empty());
                } else {
                    one_json(&output.stdout);
                    assert!(output.stderr.is_empty());
                }
            }
            let locales = std::fs::read_to_string(race.temporary.path().join("locales")).unwrap();
            assert!(locales.lines().all(|line| line == "C"), "{locales}");
        }
    }
    // 共通の git diff API を使う context は従来の AstroError と exit1 のまま。
    let repo = source_repo();
    let race = GitRace::new();
    let output = race
        .base_command(&repo, "context", "short", false)
        .arg("--git")
        .env("ASTRO_RETRY_FAILURES", "3")
        .output()
        .unwrap();
    assert_error(&output, "INVALID_REQUEST", 1);
    assert_eq!(race.attempts(), 1);
    assert_eq!(
        one_json(&output.stdout)["error"]["message"],
        "git diff failed: error: short read while indexing api.ts\n"
    );
}

#[test]
fn review_input_retry_explicit_diff_file_and_stdin_are_read_once() {
    use std::io::Write;
    for source in ["diff", "file", "stdin"] {
        let repo = source_repo();
        let race = GitRace::new();
        let mut command = race.base_command(&repo, "review", "other", false);
        match source {
            "diff" => {
                command.args(["--diff", ""]);
            }
            "file" => {
                let diff = race.temporary.path().join("input.diff");
                std::fs::write(&diff, "").unwrap();
                command.arg("--diff-file").arg(diff);
            }
            _ => {}
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        if source == "stdin" {
            child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        }
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{source}: {output:?}");
        assert!(
            one_json(&output.stdout)["impact"]["changes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(race.attempts(), 0);
    }
}

#[test]
fn review_input_retry_invalid_base_is_not_retried() {
    let repo = source_repo();
    let race = GitRace::new();
    let output = race
        .command(&repo, "latest", false)
        .args(["--base", "missing-base"])
        .output()
        .unwrap();
    assert_error(&output, "INVALID_REQUEST", 1);
    assert_eq!(race.attempts(), 1);
}
