//! CLI の基本挙動 (exit status / 出力形式 / doctor / init / skill / batch) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn cli_suppresses_broken_pipe_when_stdout_reader_drops() {
    let mut child = cargo_bin()
        .args(["symbols", "--dir", "src", "--glob", "**/*.rs"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn command");

    // パイプ先が head 等で先に終了した状況を再現する。
    drop(child.stdout.take());

    let output = child.wait_with_output().expect("failed to wait command");
    assert!(
        output.status.success(),
        "command should treat broken stdout pipe as success: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked at"),
        "broken pipe should not print a Rust panic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn doctor_returns_json() {
    let output = cargo_bin().arg("doctor").output().expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["version"], PKG_VERSION);
    let languages = json["languages"].as_array().unwrap();
    assert_eq!(languages.len(), 17);
    assert!(languages.iter().any(|lang| lang["language"] == "zig"));
    assert!(languages.iter().any(|lang| lang["language"] == "xojo"));

    // すべての対応言語が利用可能であること
    for lang in languages {
        assert!(
            lang["available"].as_bool().unwrap(),
            "Language {:?} not available",
            lang["language"]
        );
    }
}

#[test]
fn cache_works() {
    // Run ast command twice, second should be cached
    let output1 = cargo_bin()
        .args(["ast", "--path", "src/lib.rs", "--line", "0", "--col", "0"])
        .output()
        .expect("failed to run");
    assert!(output1.status.success());

    let output2 = cargo_bin()
        .args(["ast", "--path", "src/lib.rs", "--line", "0", "--col", "0"])
        .output()
        .expect("failed to run");
    assert!(output2.status.success());

    // Both should return the same result
    assert_eq!(output1.stdout, output2.stdout);
}

#[test]
fn no_cache_flag() {
    let output = cargo_bin()
        .args([
            "ast",
            "--path",
            "src/lib.rs",
            "--line",
            "0",
            "--col",
            "0",
            "--no-cache",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
}

// ---- New tests: compact/pretty, --diff, batch, MCP ----

#[test]
fn compact_output_default() {
    let output = cargo_bin()
        .args(["symbols", "--path", "src/lib.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "Default output should be a single compact JSON line"
    );

    // Should be valid JSON
    let _: serde_json::Value = serde_json::from_str(lines[0]).expect("should be valid JSON");
}

#[test]
fn pretty_output_flag() {
    let output = cargo_bin()
        .args(["symbols", "--pretty", "--path", "src/lib.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(lines.len() > 1, "Pretty output should be multi-line");

    // Should be valid JSON
    let _: serde_json::Value = serde_json::from_str(&stdout).expect("should be valid JSON");
}

#[test]
fn batch_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--paths", "src/lib.rs,src/cli.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Batch should produce 2 NDJSON lines");

    for line in &lines {
        let json: serde_json::Value =
            serde_json::from_str(line).expect("each line should be valid JSON");
        assert!(json["symbols"].is_array());
    }
}

#[test]
fn batch_with_error() {
    let output = cargo_bin()
        .args(["symbols", "--paths", "src/lib.rs,nonexistent.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Batch should produce 2 NDJSON lines");

    // 1行目は正常レスポンス
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first["symbols"].is_array());

    // 2行目は行内エラー
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["error"]["code"], "FILE_NOT_FOUND");
}

#[test]
fn batch_paths_file() {
    use std::io::Write;

    let tmp = std::env::temp_dir().join("astro_sight_paths.txt");
    let mut f = std::fs::File::create(&tmp).unwrap();
    writeln!(f, "src/lib.rs").unwrap();
    writeln!(f, "src/cli.rs").unwrap();
    drop(f);

    let output = cargo_bin()
        .args(["calls", "--paths-file", tmp.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Batch should produce 2 NDJSON lines");

    let _ = std::fs::remove_file(&tmp);
}

// ---- init サブコマンドテスト ----

#[test]
fn init_creates_config_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");

    let output = cargo_bin()
        .args(["init", "--path", config_path.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    assert!(config_path.exists(), "init が設定ファイルを作成すべき");

    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("debug = false"),
        "デフォルト設定を含むべき"
    );
}

// ---- skill-install サブコマンドテスト ----

#[test]
fn skill_install_unknown_target() {
    let output = cargo_bin()
        .args(["skill-install", "unknown-agent"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("JSON エラー出力であるべき");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown target")
    );
}

/// init / skill-install は既存 config を必要としない早期終了コマンドのため、
/// config ロードより前に処理する。壊れた既存 config を `--config` で指していても
/// init が成功する（壊れた config の再生成手段になる）ことを検証する回帰テスト。
#[test]
fn init_does_not_require_valid_existing_config() {
    let dir = tempfile::TempDir::new().unwrap();
    let bad_config = dir.path().join("bad.toml");
    let out_config = dir.path().join("generated.toml");
    std::fs::write(&bad_config, "not valid [[[").unwrap();

    let output = cargo_bin_with_explicit_config()
        .args([
            "--config",
            bad_config.to_str().unwrap(),
            "init",
            "--path",
            out_config.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "壊れた既存 config を指していても init は成功すべき: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(out_config.exists(), "config ファイルを生成すべき");
}
