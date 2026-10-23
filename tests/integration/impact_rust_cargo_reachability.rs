//! Rust の Cargo の到達性による impact の除外と、標準のマクロ・derive (Issue #117)。

use super::support::*;

/// a の `Result` を変えたときの、b と c の呼び出し元の扱い (impacted か informational に出るか)。
/// c は a に依存し、b は依存しない。
fn callers_after_change(b_source: &str, dependencies: &str) -> (bool, bool, serde_json::Value) {
    let repo = TestRepo::new();
    for package in ["a", "b", "c"] {
        repo.create_dir_all(format!("{package}/src"));
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
    );
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"sample-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "b/Cargo.toml",
        format!("[package]\nname = \"sample-b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nanyhow = \"1\"\n{dependencies}"),
    );
    repo.write(
        "c/Cargo.toml",
        "[package]\nname = \"sample-c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nsample-a = { path = \"../a\" }\n",
    );
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, ()>;\n",
    );
    repo.write("b/src/lib.rs", b_source);
    repo.write(
        "c/src/lib.rs",
        "use sample_a::Result;\npub fn caller() -> Result<()> {\n    println!(\"caller\");\n    Ok(())\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, u8>;\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "a/src/lib.rs")
        .unwrap()
        .clone();
    let lists_path = |key: &str, path: &str| {
        change[key]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == path))
    };
    let b_listed = lists_path("impacted_callers", "b/src/lib.rs")
        || lists_path("informational_callers", "b/src/lib.rs");
    let c_impacted = lists_path("impacted_callers", "c/src/lib.rs");
    (b_listed, c_impacted, context)
}

#[test]
fn impact_rust_cargo_reachability_allows_standard_macros_and_derives() {
    // b に標準のマクロと derive があっても、a に依存しない b の同名の型は impact に数えない。
    let (b_listed, c_impacted, context) = callers_after_change(
        "use anyhow::Result;\n\n#[derive(Debug)]\npub struct Marker;\n\npub fn unrelated() -> Result<()> {\n    println!(\"unrelated\");\n    Ok(())\n}\n",
        "",
    );
    assert!(!b_listed && c_impacted, "{context}");
}

#[test]
fn impact_rust_cargo_reachability_keeps_callers_for_unknown_expansions() {
    // 標準のマクロを上書きしうる外部の glob や、外部のマクロがあれば到達性で除外しない。
    for b_source in [
        "use anyhow::*;\n\npub fn unrelated() -> Result<()> {\n    println!(\"unrelated\");\n    Ok(())\n}\n",
        "use anyhow::Result;\n\npub fn unrelated() -> Result<()> {\n    anyhow::ensure!(true, \"x\");\n    Ok(())\n}\n",
    ] {
        let (b_listed, c_impacted, context) = callers_after_change(b_source, "");
        assert!(b_listed && c_impacted, "{b_source}: {context}");
    }
}

#[test]
fn impact_rust_cargo_reachability_checks_macro_arguments_and_glob_aliases() {
    let (b_listed, c_impacted, context) = callers_after_change(
        "use anyhow::Result;\npub fn unrelated() -> Result<()> {\n    let _ = vec![{ let value = 1; assert_eq!(value, 1); value }];\n    Ok(())\n}\n#[cfg(test)]\nmod tests { use super::*; #[test] fn works() { assert!(unrelated().is_ok()); } }\n",
        "",
    );
    assert!(!b_listed && c_impacted, "{context}");
    for source in [
        "use anyhow::Result;\npub use custom as origin;\npub use crate::origin as relay;\nuse crate::relay::*;\npub fn unrelated() -> Result<()> { println!(\"x\"); Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ use custom::println; println!(); 1 }]; Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ #[custom::attribute] fn generated() {} 1 }]; Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ #[derive(custom::Derive)] struct Generated; 1 }]; Ok(()) }\n",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { let _ = vec![{ macro_rules! println { () => {}; } println!(); 1 }]; Ok(()) }\n",
    ] {
        let (b_listed, c_impacted, context) = callers_after_change(source, "custom = \"1\"\n");
        assert!(b_listed && c_impacted, "{source}: {context}");
    }
    for name in ["proc_macro", "test"] {
        let dependencies = format!("{name} = {{ package = \"custom\", version = \"1\" }}\n");
        let source = format!(
            "extern crate {name} as shim;\nuse crate::shim::*;\nuse anyhow::Result;\npub fn unrelated() -> Result<()> {{ println!(\"x\"); Ok(()) }}\n"
        );
        let (b_listed, c_impacted, context) = callers_after_change(&source, &dependencies);
        assert!(b_listed && c_impacted, "{source}: {context}");
    }
}

#[test]
fn impact_rust_cargo_reachability_rejects_replaced_sysroot_in_legacy_tables() {
    let source = "use std::println;\nuse anyhow::Result;\npub fn unrelated() -> Result<()> { println!(\"x\"); Ok(()) }\n";
    let (b_listed, c_impacted, context) = callers_after_change(source, "");
    assert!(!b_listed && c_impacted, "{context}");
    for table in ["dev_dependencies", "build_dependencies"] {
        for prefix in ["", "target.'cfg(unix)'."] {
            let dependencies =
                format!("[{prefix}{table}]\nstd = {{ package = \"custom\", version = \"1\" }}\n");
            let (b_listed, c_impacted, context) = callers_after_change(source, &dependencies);
            assert!(b_listed && c_impacted, "{dependencies}: {context}");
        }
    }
    let (b_listed, c_impacted, context) = callers_after_change(
        "use std::include as println;\nprintln!(\"../../a/src/lib.rs\");\npub fn unrelated() -> Result { Result }\n",
        "",
    );
    // include! が作る型は解決しないが、到達性によって候補から消してはいけない。
    let b_low_confidence = context["changes"][0]["low_confidence_callers"]
        .as_array()
        .is_some_and(|callers| {
            callers
                .iter()
                .any(|caller| caller["path"] == "b/src/lib.rs")
        });
    assert!((b_listed || b_low_confidence) && c_impacted, "{context}");
}
