//! サイズ上限つき入力読み込みと git revision 検証のテスト。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
#[allow(unused_imports)]
use std::collections::HashSet;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::Cursor;
#[allow(unused_imports)]
use std::process::Command;

#[test]
fn read_to_string_limited_accepts_small_input() {
    let text = read_to_string_limited(Cursor::new(b"ok".to_vec()), 4, "stdin").unwrap();
    assert_eq!(text, "ok");
}

#[test]
fn read_to_string_limited_rejects_oversized_input() {
    let err = read_to_string_limited(Cursor::new(b"abcde".to_vec()), 4, "stdin")
        .expect_err("oversized input should fail");

    assert!(err.to_string().contains("exceeds maximum size"));
}

#[test]
fn read_bytes_limited_and_drain_reports_full_size() {
    let err = read_bytes_limited_and_drain(Cursor::new(vec![b'a'; 10]), 4, "git diff output")
        .expect_err("oversized input should fail");

    assert!(err.to_string().contains("10 bytes > 4 bytes"));
}

#[test]
fn read_to_string_limited_rejects_invalid_utf8() {
    let err = read_to_string_limited(Cursor::new(vec![0xff]), 4, "stdin")
        .expect_err("invalid utf-8 should fail");

    assert!(err.to_string().contains("not valid UTF-8"));
}

#[test]
fn read_paths_file_limited_trims_blank_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("paths.txt");
    fs::write(&path, " src/main.rs \n\nCargo.toml\n").expect("write paths file");

    let paths =
        read_paths_file_limited(path.to_str().expect("utf-8 path"), 1024).expect("read paths");

    assert_eq!(paths, vec!["src/main.rs", "Cargo.toml"]);
}

#[test]
fn read_paths_file_limited_rejects_oversized_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("paths.txt");
    fs::write(&path, "abcde").expect("write paths file");

    let err = read_paths_file_limited(path.to_str().expect("utf-8 path"), 4)
        .expect_err("oversized paths-file should fail");

    assert!(err.to_string().contains("exceeds maximum size"));
}

#[test]
fn validate_git_revision_accepts_normal_values() {
    assert!(validate_git_revision("HEAD", "--base").is_ok());
    assert!(validate_git_revision("HEAD^", "--base").is_ok());
    assert!(validate_git_revision("main", "--base").is_ok());
    assert!(validate_git_revision("origin/main", "--base").is_ok());
    assert!(validate_git_revision("feature/foo", "--base").is_ok());
    assert!(validate_git_revision("abc1234", "--base").is_ok());
    assert!(validate_git_revision("v1.0.0", "--base").is_ok());
}

// `--output=/path` 等のオプション注入を拒否する
#[test]
fn validate_git_revision_rejects_option_prefix() {
    let err = validate_git_revision("--output=/tmp/pwn", "--base")
        .expect_err("option-like base should be rejected");
    assert!(err.to_string().contains("must not start with '-'"));

    let err = validate_git_revision("-p", "--base").expect_err("short option should be rejected");
    assert!(err.to_string().contains("must not start with '-'"));
}

#[test]
fn validate_git_revision_rejects_empty() {
    let err = validate_git_revision("", "--base").expect_err("empty revision should be rejected");
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn validate_git_revision_rejects_nul() {
    let err =
        validate_git_revision("HEAD\0foo", "--base").expect_err("NUL byte should be rejected");
    assert!(err.to_string().contains("must not contain NUL"));
}
