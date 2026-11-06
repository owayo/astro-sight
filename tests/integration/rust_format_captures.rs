use super::support::*;

#[test]
fn rust_format_captures_decode_names_without_raw_prefilter_hits() {
    let repo = TestRepo::new();
    repo.write("api.rs", "pub const VALUE: u32 = 7;\n");
    repo.write(
        "caller.rs",
        "use crate::api::*;\npub fn render() -> String { format!(\"{VA\\u{4c}UE}\") }\n",
    );
    let refs = repo.run_json("refs", &["--name", "VALUE"]);
    assert_eq!(refs["refs"].as_array().unwrap().len(), 2, "{refs}");
    let batch = repo.run_json("refs", &["--names", "VALUE"]);
    assert_eq!(batch["refs"], refs["refs"]);
    assert!(
        refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "caller.rs" && r["kind"] == "ref")
    );
    let dead = repo.run_json("dead-code", &[]);
    assert!(
        dead["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "VALUE"),
        "{dead}"
    );
}

#[test]
fn rust_format_captures_are_references_and_keep_constants_live() {
    let repo = TestRepo::new();
    repo.write("lib.rs", "pub const ENDPOINT: &str = \"https://example.invalid\";\npub fn render() -> String { format!(\"{ENDPOINT}/download\") }\n");
    let single = repo.run_json("refs", &["--name", "ENDPOINT"]);
    assert_eq!(single["refs"].as_array().unwrap().len(), 2, "{single}");
    assert_eq!(single["refs"][1]["ln"], 1);
    assert_eq!(single["refs"][1]["col"], 38);
    assert_eq!(single["refs"][1]["kind"], "ref");
    let batch = repo.run_json("refs", &["--names", "ENDPOINT"]);
    assert_eq!(batch["refs"], single["refs"]);
    let dead = repo.run_json("dead-code", &[]);
    assert!(
        dead["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "ENDPOINT"),
        "{dead}"
    );
    repo.init_git();
    repo.commit_all("base");
    repo.write("lib.rs", "pub const ENDPOINT: &str = \"https://example.invalid/v2\";\npub fn render() -> String { format!(\"{ENDPOINT}/download\") }\n");
    let review = repo.run_json("review", &["--git"]);
    assert!(
        review["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "ENDPOINT"),
        "{review}"
    );
}

#[test]
fn rust_format_captures_preserve_original_unicode_crlf_positions() {
    let repo = TestRepo::new();
    let source = "pub const TARGET: u32 = 1;\r\nfn render() {\r\n    println!(r#\"日本語\n{TARGET:>width$.precision$} {{TARGET}}\"#);\r\n}\r\n";
    repo.write("lib.rs", source);
    for (name, row, col) in [("TARGET", 3, 1), ("width", 3, 9), ("precision", 3, 16)] {
        let output = repo.run_json("refs", &["--name", name]);
        let references: Vec<_> = output["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == "ref")
            .collect();
        assert_eq!(references.len(), 1, "{name}: {output}");
        assert_eq!(references[0]["ln"], row);
        assert_eq!(references[0]["col"], col);
    }
}

/// 行継続 (`\` と改行と続く空白) をはさんで綴った暗黙キャプチャを、名前の生の綴りが無い
/// ファイルでも参照として数える (Issue #248)。refs の単一名・一括、dead-code、context の
/// 呼び出し元がそろい、位置は元のソースの名前の先頭を指す。
#[test]
fn rust_format_captures_decode_line_continuations_without_raw_prefilter_hits() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"continued\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod api;\npub mod caller;\npub mod caller_crlf;\n",
    );
    repo.write(
        "src/api.rs",
        "pub const TOKEN: u32 = 7;\npub const WIDTH: usize = 4;\n",
    );
    repo.write(
        "src/caller.rs",
        "use crate::api::*;\npub fn render() -> String {\n    format!(\"{TO\\\n        KEN:>WI\\\n    DTH$}\")\n}\n",
    );
    repo.write(
        "src/caller_crlf.rs",
        "use crate::api::*;\r\npub fn render_crlf() -> String {\r\n    format!(\"{\\x54O\\\r\n\tKEN}\")\r\n}\r\n",
    );
    for (name, expected) in [
        (
            "TOKEN",
            vec![("src/caller.rs", 2, 14), ("src/caller_crlf.rs", 2, 14)],
        ),
        ("WIDTH", vec![("src/caller.rs", 3, 13)]),
    ] {
        let single = repo.run_json("refs", &["--name", name]);
        let references: Vec<_> = single["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == "ref")
            .map(|r| {
                (
                    r["path"].as_str().unwrap().to_string(),
                    r["ln"].as_u64().unwrap(),
                    r["col"].as_u64().unwrap(),
                )
            })
            .collect();
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(path, ln, col)| (path.to_string(), ln, col))
            .collect();
        assert_eq!(references, expected, "{name}: {single}");
        let batch = repo.run_json("refs", &["--names", name]);
        assert_eq!(batch["refs"], single["refs"], "{name}");
    }
    let dead = repo.run_json("dead-code", &[]);
    assert!(
        dead["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "TOKEN" && s["name"] != "WIDTH"),
        "{dead}"
    );

    repo.init_git();
    repo.commit_all("base");
    repo.write(
        "src/api.rs",
        "pub const TOKEN: u64 = 7;\npub const WIDTH: usize = 4;\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let callers: Vec<_> = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| {
            change["impacted_callers"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|caller| caller["path"].as_str().unwrap().to_string())
        .collect();
    for path in ["src/caller.rs", "src/caller_crlf.rs"] {
        assert!(
            callers.iter().any(|caller| caller == path),
            "{path}: {context}"
        );
    }
}
