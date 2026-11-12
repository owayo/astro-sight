use super::support::{TestRepo, cargo_bin};

#[test]
fn c_conditional_linkage_cli_preserves_declarations_references_and_diagnostics() {
    let repo = TestRepo::new();
    let file_json = |command: &str, args: &[&str]| -> serde_json::Value {
        let output = cargo_bin()
            .arg(command)
            .args(args)
            .current_dir(repo.root())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    for (index, guard) in ["#ifdef __cplusplus", "#if defined(__cplusplus)"]
        .iter()
        .enumerate()
    {
        let header = format!(
            "/* 日本語 */\r\n#ifndef SAMPLE_H\r\n#define SAMPLE_H\r\n{guard}\r\nextern \"C\" {{\r\n#endif\r\nvoid sample_run(void);\r\nstatic inline int sample_value(int x) {{ return x; }}\r\n{guard}\r\n}}\r\n#endif\r\n#endif\r\n"
        );
        let path = format!("sample{index}.h");
        repo.write(&path, &header);
        let symbols = file_json("symbols", &["--path", &path, "--no-cache"]);
        assert_eq!(symbols["lang"], "c");
        assert!(symbols.get("diagnostics").is_none(), "{symbols}");
        let names: Vec<_> = symbols["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["sample_value"]);
        let ast = file_json(
            "ast",
            &["--path", &path, "--line", "6", "--col", "5", "--no-cache"],
        );
        assert!(ast.get("diagnostics").is_none(), "{ast}");
        assert_eq!(ast["ast"][0]["kind"], "identifier");
        assert_eq!(ast["ast"][0]["range"], serde_json::json!([6, 5, 6, 15]));
        let refs = repo.run_json("refs", &["--name", "sample_run", "--glob", &path]);
        assert_eq!(refs["refs"].as_array().unwrap().len(), 1);
        assert_eq!(refs["refs"][0]["ln"], 6);
        assert_eq!(refs["refs"][0]["col"], 5);
        let guards = repo.run_json("refs", &["--name", "__cplusplus", "--glob", &path]);
        assert_eq!(guards["refs"].as_array().unwrap().len(), 2);
        repo.write(&path, header.replace("sample_run(void)", "sample_run( ;"));
        let broken = file_json("symbols", &["--path", &path, "--no-cache"]);
        assert!(
            broken["diagnostics"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "{broken}"
        );
    }
}
