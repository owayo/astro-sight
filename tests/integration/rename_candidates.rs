use super::support::*;

fn repository() -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='rename-sample'\nversion='0.1.0'\nedition='2024'\n",
    );
    repo.write(
        "src/lib.rs",
        "pub fn double(v: u32) -> u32 { v * 2 }\npub fn invoke() -> u32 { double(3) }\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        "pub fn twice(v: u32) -> u32 { v * 2 }\npub fn invoke() -> u32 { twice(3) }\n",
    );
    repo
}

#[test]
fn rename_candidates_json_hook_and_toon_preserve_existing_classifications() {
    let repo = repository();
    let json = repo.run_json("review", &["--git", "--format", "json"]);
    assert_eq!(
        json["rename_candidates"][0]["old"]["name"], "double",
        "{json}"
    );
    assert_eq!(json["rename_candidates"][0]["new"]["name"], "twice");
    assert_eq!(json["rename_candidates"][0]["old"]["line"], 0);
    assert_eq!(json["rename_candidates"][0]["old"]["column"], 7);
    assert_eq!(json["api_changes"]["added"], serde_json::json!([]));
    assert_eq!(json["api_changes"]["removed"], serde_json::json!([]));
    assert_eq!(json["api_changes"]["modified"], serde_json::json!([]));
    let symbols = json["impact"]["changes"][0]["affected_symbols"]
        .as_array()
        .unwrap();
    assert!(
        symbols
            .iter()
            .any(|s| s["name"] == "twice" && s["change_type"] == "added")
    );
    assert!(
        symbols
            .iter()
            .any(|s| s["name"] == "invoke" && s["change_type"] == "modified")
    );

    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(hook["rename_candidates"], json["rename_candidates"]);
    assert_eq!(
        hook["blocking_categories"],
        serde_json::json!(["dead"]),
        "{hook}"
    );
    assert_eq!(output.status.code(), Some(1));

    let output = cargo_bin()
        .args(["review", "--git", "--format", "toon", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert!(output.status.success());
    let toon = String::from_utf8(output.stdout).unwrap();
    assert!(toon.contains("rename_candidates"), "{toon}");
    assert!(toon.contains("name_independent_ast_equal"), "{toon}");
}

#[test]
fn rename_candidates_staged_reads_index_and_separates_reexport_evidence() {
    let repo = repository();
    repo.write("src/lib.rs", "pub fn twice(v: u32) -> u32 { v * 2 }\npub fn invoke() -> u32 { twice(3) }\npub use self::twice as double;\n");
    repo.stage_all();
    repo.write("src/lib.rs", "pub fn unrelated(v: u32) -> u32 { v * 9 }\n");
    let json = repo.run_json("review", &["--git", "--staged", "--format", "json"]);
    assert_eq!(
        json["rename_candidates"][0]["new"]["name"], "twice",
        "{json}"
    );
    assert_eq!(
        json["rename_candidates"][0]["old_api_presence"]["status"],
        "direct_reexport_present"
    );
    assert_eq!(
        json["rename_candidates"][0]["old_api_presence"]["compatibility"],
        "unverified"
    );
    let unstaged = repo.run_json("review", &["--git", "--format", "json"]);
    assert!(unstaged.get("rename_candidates").is_none(), "{unstaged}");

    for after in [
        "pub fn twice(v: u32) -> u32 { v * 2 }\n#[cfg(feature = \"legacy\")]\n// comment\npub use self::twice as double;\n",
        "#[cfg(feature = \"legacy\")]\npub fn twice(v: u32) -> u32 { v * 2 }\npub use self::twice as double;\n",
        "#![cfg(feature = \"legacy\")]\npub fn twice(v: u32) -> u32 { v * 2 }\npub use self::twice as double;\n",
    ] {
        repo.write("src/lib.rs", after);
        let conditional = repo.run_json("review", &["--git", "--format", "json"]);
        assert_eq!(
            conditional["rename_candidates"][0]["old_api_presence"]["status"], "unverified",
            "{conditional}"
        );
    }
}
