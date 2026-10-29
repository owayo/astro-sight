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
