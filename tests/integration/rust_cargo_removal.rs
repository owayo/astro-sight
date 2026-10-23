//! Rust のクレートを消したときの api.rm と、Cargo の依存で届かない同名の関数 (Issue #111)。

use super::support::*;

const OLD_RUN: &str = "pub fn run() -> u32 {\n    7\n}\n";
const ENGINE_RUN: &str =
    "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    run()\n}\n";
// #110 が追わない use 経由の参照を残し、Cargo の証明を使う対照にする。
const ENGINE_IMPORT_RUN: &str = "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    run()\n}\n\nmod indirect {\n    use super::run;\n    pub fn call() -> u32 {\n        run()\n    }\n}\n";

fn package(name: &str, extra: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}")
}

/// old_app と engine の workspace をコミットし、old_app をまるごと消して members から外す。
fn remove_old_app(engine_manifest: &str, engine_source: &str) -> TestRepo {
    remove_old_app_with_files(engine_manifest, engine_source, &[])
}

fn remove_old_app_with_files(
    engine_manifest: &str,
    engine_source: &str,
    files: &[(&str, &str)],
) -> TestRepo {
    let repo = TestRepo::new();
    for package in ["old_app", "engine"] {
        repo.create_dir_all(format!("{package}/src"));
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"old_app\", \"engine\"]\nresolver = \"2\"\n",
    );
    repo.write("old_app/Cargo.toml", package("old_app", ""));
    repo.write("old_app/src/lib.rs", OLD_RUN);
    repo.write("engine/Cargo.toml", engine_manifest);
    repo.write("engine/src/lib.rs", engine_source);
    for (path, source) in files {
        repo.write(path, source);
    }
    repo.init_git();
    repo.commit_all("base");
    repo.git(["rm", "-r", "-q", "old_app"]);
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"engine\"]\nresolver = \"2\"\n",
    );
    repo
}

/// `review --git --hook` の終了コードと、stderr の hook JSON で `run` が入った api のキー。
fn review_hook(repo: &TestRepo, extra: &[&str]) -> (Option<i32>, String) {
    review_hook_with_env(repo, extra, &[])
}

fn review_hook_with_env(
    repo: &TestRepo,
    extra: &[&str],
    variables: &[(&str, &std::ffi::OsStr)],
) -> (Option<i32>, String) {
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .args(extra)
        .envs(variables.iter().copied())
        .output()
        .unwrap();
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|_| {
        panic!("{}", String::from_utf8_lossy(&output.stderr));
    });
    let keys: Vec<&str> = ["rm", "rm_dead"]
        .into_iter()
        .filter(|key| {
            hook["api"][key]
                .as_array()
                .is_some_and(|symbols| symbols.iter().any(|symbol| symbol["n"] == "run"))
        })
        .collect();
    assert_eq!(keys.len(), 1, "{hook}");
    (output.status.code(), keys[0].to_owned())
}

#[test]
fn rust_cargo_removal_hook_ignores_same_named_function_in_unrelated_crate() {
    for source in [ENGINE_RUN, ENGINE_IMPORT_RUN] {
        let repo = remove_old_app(&package("engine", ""), source);
        assert_eq!(review_hook(&repo, &[]), (Some(0), "rm_dead".to_owned()));
        // staged は元の Cargo 設定の場所を確定できない。#110 の証拠だけで解決できる形を区別する。
        repo.stage_all();
        assert_eq!(
            review_hook(&repo, &["--staged"]),
            if source == ENGINE_RUN {
                (Some(0), "rm_dead".to_owned())
            } else {
                (Some(1), "rm".to_owned())
            }
        );
    }
}

#[test]
fn rust_cargo_removal_hook_checks_cargo_home_and_compiler_environment() {
    let repo = remove_old_app(&package("engine", ""), ENGINE_IMPORT_RUN);
    let home = tempfile::tempdir().unwrap();
    let variables = [("CARGO_HOME", home.path().as_os_str())];
    let config = home.path().join("config.toml");
    for (source, dead) in [
        ("[build]\njobs = 1\n", true),
        (
            "[build]\nrustflags = ['--extern', 'injected=libalpha.rlib']\n",
            false,
        ),
    ] {
        std::fs::write(&config, source).unwrap();
        assert_eq!(
            review_hook_with_env(&repo, &[], &variables),
            if dead {
                (Some(0), "rm_dead".to_owned())
            } else {
                (Some(1), "rm".to_owned())
            },
            "{source}"
        );
    }
    std::fs::remove_file(config).unwrap();
    for (name, value) in [
        ("RUSTFLAGS", "--extern injected=libalpha.rlib"),
        (
            "CARGO_ENCODED_RUSTFLAGS",
            "--extern\u{1f}injected=libalpha.rlib",
        ),
        (
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            "--extern=injected=libalpha.rlib",
        ),
        ("RUSTC_WRAPPER", "other-rustc"),
        ("RUSTFLAGS", "@response-file"),
    ] {
        assert_eq!(
            review_hook_with_env(
                &repo,
                &[],
                &[
                    ("CARGO_HOME", home.path().as_os_str()),
                    (name, std::ffi::OsStr::new(value))
                ],
            ),
            (Some(1), "rm".to_owned()),
            "{name}={value}"
        );
    }
    assert_eq!(
        review_hook_with_env(&repo, &[], &variables),
        (Some(0), "rm_dead".to_owned())
    );
}

#[test]
fn rust_cargo_removal_hook_without_same_named_function_stays_dead() {
    let repo = remove_old_app(
        &package("engine", ""),
        "pub fn drive() -> u32 {\n    1\n}\n",
    );
    assert_eq!(review_hook(&repo, &[]), (Some(0), "rm_dead".to_owned()));
}

#[test]
fn rust_cargo_removal_hook_keeps_blocking_for_remaining_dependency() {
    for dependency in [
        "[dependencies]\nold_app = { path = \"../old_app\" }\n",
        "[dev-dependencies]\nold_app = { path = \"../old_app\" }\n",
        "[build-dependencies]\nold_app = { path = \"../old_app\" }\n",
    ] {
        let repo = remove_old_app(&package("engine", dependency), ENGINE_IMPORT_RUN);
        assert_eq!(
            review_hook(&repo, &[]),
            (Some(1), "rm".to_owned()),
            "{dependency}"
        );
        // 依存も同時に消しても、変更前の engine は old_app を使えた。
        repo.write("engine/Cargo.toml", package("engine", ""));
        assert_eq!(
            review_hook(&repo, &[]),
            (Some(1), "rm".to_owned()),
            "{dependency} (removed)"
        );
    }
}

#[test]
fn rust_cargo_removal_hook_keeps_blocking_through_intermediate_crate() {
    let repo = TestRepo::new();
    for package in ["old_app", "mid", "engine"] {
        repo.create_dir_all(format!("{package}/src"));
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"old_app\", \"mid\", \"engine\"]\nresolver = \"2\"\n",
    );
    repo.write("old_app/Cargo.toml", package("old_app", ""));
    repo.write("old_app/src/lib.rs", OLD_RUN);
    repo.write(
        "mid/Cargo.toml",
        package(
            "mid",
            "[dependencies]\nold_app = { path = \"../old_app\" }\n",
        ),
    );
    repo.write("mid/src/lib.rs", "pub fn middle() -> u32 {\n    2\n}\n");
    repo.write(
        "engine/Cargo.toml",
        package("engine", "[dependencies]\nmid = { path = \"../mid\" }\n"),
    );
    repo.write("engine/src/lib.rs", ENGINE_IMPORT_RUN);
    repo.init_git();
    repo.commit_all("base");
    repo.git(["rm", "-r", "-q", "old_app"]);
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"mid\", \"engine\"]\nresolver = \"2\"\n",
    );
    repo.write("mid/Cargo.toml", package("mid", ""));
    assert_eq!(review_hook(&repo, &[]), (Some(1), "rm".to_owned()));
}

#[test]
fn rust_cargo_removal_hook_keeps_blocking_for_exported_symbol() {
    for old_source in [
        "#[no_mangle]\npub extern \"C\" fn run() -> u32 {\n    7\n}\n",
        "pub extern \"C\" fn run() -> u32 {\n    7\n}\n",
        "#[old_macros::exported]\npub fn run() -> u32 {\n    7\n}\n",
    ] {
        let repo = TestRepo::new();
        for package in ["old_app", "engine"] {
            repo.create_dir_all(format!("{package}/src"));
        }
        repo.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"old_app\", \"engine\"]\nresolver = \"2\"\n",
        );
        repo.write("old_app/Cargo.toml", package("old_app", ""));
        repo.write("old_app/src/lib.rs", old_source);
        repo.write("engine/Cargo.toml", package("engine", ""));
        repo.write("engine/src/lib.rs", ENGINE_IMPORT_RUN);
        repo.init_git();
        repo.commit_all("base");
        repo.git(["rm", "-r", "-q", "old_app"]);
        repo.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"engine\"]\nresolver = \"2\"\n",
        );
        assert_eq!(
            review_hook(&repo, &[]),
            (Some(1), "rm".to_owned()),
            "{old_source}"
        );
    }
}

#[test]
fn rust_cargo_removal_hook_reads_links_from_the_staged_tree() {
    // staged の解析が使う一時リポジトリは symlink を書き出さない。index にだけある
    // ディレクトリへの symlink も、変更後のツリーの要素として証明を止める。
    let repo = remove_old_app(&package("engine", ""), ENGINE_IMPORT_RUN);
    repo.stage_all();
    let mut child = std::process::Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo.root())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"engine/src").unwrap();
    let object = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    repo.git([
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("120000,{},docs", object.trim()),
    ]);
    assert_eq!(
        review_hook(&repo, &["--staged"]),
        (Some(1), "rm".to_owned())
    );
}

#[test]
fn rust_cargo_removal_hook_keeps_blocking_for_lfs_pointer_in_worktree() {
    let lfs = "version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 12\n";
    let crlf = lfs.replace('\n', "\r\n");
    for (source, expected) in [
        ("pub const VALUE: u32 = 1;\n", (Some(0), "rm_dead")),
        (lfs, (Some(1), "rm")),
        (crlf.as_str(), (Some(1), "rm")),
    ] {
        let repo = remove_old_app_with_files(
            &package("engine", ""),
            &format!("pub mod data;\n{ENGINE_IMPORT_RUN}"),
            &[("engine/src/data.rs", "pub const VALUE: u32 = 1;\n")],
        );
        repo.write("engine/src/data.rs", source);
        assert_eq!(
            review_hook(&repo, &[]),
            (expected.0, expected.1.to_owned()),
            "{source}"
        );
        repo.stage_all();
        assert_eq!(
            review_hook(&repo, &["--staged"]),
            (Some(1), "rm".to_owned()),
            "staged: {source}"
        );
    }
}

#[test]
fn rust_cargo_removal_hook_keeps_refs_and_dead_code_occurrences() {
    // 削除判定だけが変わり、refs の出現と dead-code の対象は変えない。
    for (source, count) in [(ENGINE_RUN, 2), (ENGINE_IMPORT_RUN, 4)] {
        let repo = remove_old_app(&package("engine", ""), source);
        assert_eq!(review_hook(&repo, &[]), (Some(0), "rm_dead".to_owned()));
        let refs = repo.run_json("refs", &["--name", "run"]);
        let paths: Vec<&str> = refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| reference["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["engine/src/lib.rs"; count], "{refs}");
        let dead = repo.run_json("dead-code", &[]);
        assert!(
            dead["dead_symbols"]
                .as_array()
                .is_none_or(|symbols| symbols.iter().all(|symbol| symbol["name"] != "run")),
            "{dead}"
        );
    }
}

#[test]
fn rust_cargo_removal_hook_allows_standard_macros_and_derives() {
    // 実際のワークスペースによくある標準のマクロ・derive・テストのモジュールでは証明を続ける
    // (Issue #117)。
    let engine = "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    println!(\"drive\");\n    run()\n}\n\n#[derive(Debug, Clone)]\npub struct Marker;\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn drives() {\n        assert_eq!(drive(), 1);\n    }\n}\n\nmod indirect {\n    use super::run;\n    pub fn call() -> u32 {\n        run()\n    }\n}\n";
    let repo = remove_old_app(&package("engine", ""), engine);
    assert_eq!(review_hook(&repo, &[]), (Some(0), "rm_dead".to_owned()));
    // import 経由の未解決参照は Cargo の証明を要し、staged では保守的に残す。
    repo.stage_all();
    assert_eq!(
        review_hook(&repo, &["--staged"]),
        (Some(1), "rm".to_owned())
    );
    // 外部のマクロは展開で別 package のソースを取り込みうるので、今と同じく止まる。
    let repo = remove_old_app(
        &package("engine", ""),
        &format!("{engine}\npub fn fail() -> u32 {{\n    anyhow::bail!(\"x\")\n}}\n"),
    );
    assert_eq!(review_hook(&repo, &[]), (Some(1), "rm".to_owned()));
}

#[test]
fn rust_cargo_removal_hook_keeps_unproven_standard_macro_scopes() {
    for source in [
        format!(
            "pub use custom as origin;\npub use crate::origin as relay;\nuse crate::relay::*;\n{ENGINE_IMPORT_RUN}\npub fn show() {{ println!(\"x\"); }}\n"
        ),
        format!(
            "{ENGINE_IMPORT_RUN}\npub fn show() {{ let _ = vec![{{ use custom::println; println!(); 1 }}]; }}\n"
        ),
        format!(
            "{ENGINE_IMPORT_RUN}\npub fn show() {{ let _ = vec![{{ #[custom::attribute] fn generated() {{}} 1 }}]; }}\n"
        ),
        format!(
            "{ENGINE_IMPORT_RUN}\npub fn show() {{ let _ = vec![{{ #[derive(custom::Derive)] struct Generated; 1 }}]; }}\n"
        ),
        format!(
            "{ENGINE_IMPORT_RUN}\npub fn show() {{ let _ = vec![{{ macro_rules! println {{ () => {{}}; }} println!(); 1 }}]; }}\n"
        ),
    ] {
        let repo = remove_old_app(
            &package("engine", "[dependencies]\ncustom = \"1\"\n"),
            &source,
        );
        assert_eq!(
            review_hook(&repo, &[]),
            (Some(1), "rm".to_owned()),
            "{source}"
        );
    }
    for name in ["proc_macro", "test"] {
        let manifest = package(
            "engine",
            &format!("[dependencies]\n{name} = {{ package = \"custom\", version = \"1\" }}\n"),
        );
        let source = format!(
            "extern crate {name} as shim;\nuse crate::shim::*;\n{ENGINE_IMPORT_RUN}\npub fn show() {{ println!(\"x\"); }}\n"
        );
        let repo = remove_old_app(&manifest, &source);
        assert_eq!(
            review_hook(&repo, &[]),
            (Some(1), "rm".to_owned()),
            "{source}"
        );
    }
}

#[test]
fn rust_cargo_removal_hook_rejects_replaced_sysroot_in_legacy_tables() {
    let source = format!(
        "use std::println;\n{ENGINE_IMPORT_RUN}\n#[cfg(test)] pub fn show() {{ println!(\"x\"); }}\n"
    );
    let repo = remove_old_app(&package("engine", ""), &source);
    assert_eq!(review_hook(&repo, &[]), (Some(0), "rm_dead".to_owned()));
    for table in ["dev_dependencies", "build_dependencies"] {
        for prefix in ["", "target.'cfg(unix)'."] {
            let dependencies =
                format!("[{prefix}{table}]\nstd = {{ package = \"custom\", version = \"1\" }}\n");
            let repo = remove_old_app(&package("engine", &dependencies), &source);
            assert_eq!(
                review_hook(&repo, &[]),
                (Some(1), "rm".to_owned()),
                "{dependencies}"
            );
        }
    }
}
