//! 変更した private 定数と同名の、別の所有元の定数を使う参照 (Issue #260)。
//!
//! 変更元の関数の本体に標準でないマクロがあると、別のファイルのコードを変更元の block の中へ
//! 取り込める。参照ファイルの位置からの証明 (葉モジュール・private の部分木) は使えないので、
//! 参照自身の束縛 (直下の宣言・crate 起点の import) で別の所有元と確かめられた参照だけを除く。

use super::support::*;

const SOURCE: &str = "pub const SCHEMA_VERSION: u32 = 2;\n\npub fn readable(schema: u32) -> bool {\n    schema <= SCHEMA_VERSION\n}\n\npub fn describe() -> String {\n    format!(\"format {SCHEMA_VERSION}\")\n}\n\npub fn debug() {\n    println!(\"{SCHEMA_VERSION:?}\");\n}\n";

const CATALOG: &str = "use crate::source::SCHEMA_VERSION;\n\npub fn check(schema: u32) -> Result<(), String> {\n    if schema > SCHEMA_VERSION {\n        return Err(format!(\"format {schema} is newer than {SCHEMA_VERSION}\"));\n    }\n    Ok(())\n}\n";

fn store_source(call: &str) -> String {
    format!(
        "const SCHEMA_VERSION: i32 = 2;\n\npub fn needs_migration(version: i32) -> bool {{\n    version < SCHEMA_VERSION\n}}\n\npub fn row(id: i64) -> Vec<i64> {{\n    {call}\n}}\n"
    )
}

/// `store` の `const SCHEMA_VERSION: i32 = 2;` を 3 に変えた作業ツリーを作る。`files` は既定の
/// ファイルを上書きできる。
fn constants_repo(store: &str, files: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod catalog;\npub mod source;\npub mod store;\n",
    );
    repo.write("src/store.rs", store);
    repo.write("src/source.rs", SOURCE);
    repo.write("src/catalog.rs", CATALOG);
    for (path, content) in files {
        if let Some(parent) = std::path::Path::new(path).parent() {
            repo.create_dir_all(parent);
        }
        repo.write(path, content);
    }
    repo.init_git();
    repo.commit_all("initial");
    let changed = store.replace(
        "const SCHEMA_VERSION: i32 = 2;",
        "const SCHEMA_VERSION: i32 = 3;",
    );
    assert_ne!(changed, store);
    repo.write("src/store.rs", changed);
    repo
}

/// 振り分け先を問わず、`path` の caller の行 (0 起点)。
fn caller_lines(context: &serde_json::Value, bucket: Option<&str>, path: &str) -> Vec<u64> {
    let mut lines = Vec::new();
    for change in context["changes"].as_array().unwrap() {
        for name in [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers",
        ] {
            if bucket.is_some_and(|bucket| bucket != name) {
                continue;
            }
            for caller in change[name].as_array().into_iter().flatten() {
                if caller["path"] == path {
                    lines.push(caller["line"].as_u64().unwrap());
                }
            }
        }
    }
    lines.sort_unstable();
    lines
}

fn line_of(source: &str, needle: &str) -> u64 {
    source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap() as u64
}

#[test]
fn impact_rust_same_named_constants_use_their_own_bindings_beside_body_macros() {
    // `vec!` は標準のマクロで、修正前から葉モジュールの証明で除けていた対照。
    for call in ["params![id]", "thing!(id)", "vec![id]"] {
        let repo = constants_repo(&store_source(call), &[]);
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{call}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let context = repo.run_json("context", &["--git"]);
        for path in ["src/source.rs", "src/catalog.rs"] {
            assert!(
                caller_lines(&context, None, path).is_empty(),
                "{call}: {path}: {context}"
            );
        }

        // refs は暗黙キャプチャの出現をそのまま返す (除くのは impact の側だけ)。
        let refs = repo.run_json("refs", &["--name", "SCHEMA_VERSION"]);
        let found =
            |path: &str, line: u64| {
                refs["refs"].as_array().unwrap().iter().any(|entry| {
                    entry["path"] == path && entry["ln"] == line && entry["kind"] == "ref"
                })
            };
        for (path, source, needle) in [
            ("src/source.rs", SOURCE, "format!("),
            ("src/source.rs", SOURCE, "println!("),
            ("src/catalog.rs", CATALOG, "format!("),
        ] {
            assert!(found(path, line_of(source, needle)), "{call}: {refs}");
        }

        // index のスナップショットには作業ツリーの束縛の証明を使わない。
        if call == "params![id]" {
            repo.stage_all();
            let staged = repo.run_json("context", &["--git", "--staged"]);
            assert_eq!(
                caller_lines(&staged, Some("impacted_callers"), "src/catalog.rs"),
                [line_of(CATALOG, "if schema"), line_of(CATALOG, "format!(")],
                "{staged}"
            );
            assert!(
                caller_lines(&staged, Some("impacted_callers"), "src/source.rs")
                    .contains(&line_of(SOURCE, "format!(")),
                "{staged}"
            );
        }
    }
}

/// 残すべき参照の対照。`kept` は `impacted_callers` に残る行、`excluded` はどの振り分け先にも
/// 出ないファイル。
struct Case<'a> {
    name: &'a str,
    store: &'a str,
    files: Vec<(&'a str, &'a str)>,
    kept: Vec<(&'a str, Vec<u64>)>,
    excluded: Vec<&'a str>,
}

#[test]
fn impact_rust_same_named_constants_keep_unproven_bindings() {
    let params = store_source("params![id]");
    let child_store = format!("{params}\npub mod child;\n");
    let plain_child_store = format!("{}\npub mod child;\n", store_source("vec![id]"));
    let child = "use super::SCHEMA_VERSION;\n\npub fn label() -> String {\n    format!(\"v{SCHEMA_VERSION}\")\n}\n";
    let with_report = "pub mod catalog;\npub mod report;\npub mod source;\npub mod store;\n";
    let statement_macro = "pub const SCHEMA_VERSION: u8 = 1;\n\npub fn shadowed() -> String {\n    make_binding!();\n    format!(\"{SCHEMA_VERSION}\")\n}\n";
    let shadowed_format = "macro_rules! format { ($($t:tt)*) => { String::new() }; }\n\npub const SCHEMA_VERSION: u8 = 1;\n\npub fn label() -> String {\n    format!(\"{SCHEMA_VERSION}\")\n}\n";
    let nested_format = "pub const SCHEMA_VERSION: u8 = 1;\n\npub fn labels() -> Vec<String> {\n    vec![format!(\"{SCHEMA_VERSION}\")]\n}\n";
    let std_path = "pub const SCHEMA_VERSION: u8 = 1;\n\npub fn label() -> String {\n    std::format!(\"{SCHEMA_VERSION}\")\n}\n";
    // 型だけを import すると、取り込まれた先の値の参照は変更した定数へ解決される。
    let inclusion_store = "const SCHEMA_VERSION: i32 = 2;\n\nmacro_rules! catalog_items {\n    () => {\n        include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/catalog.rs\"));\n    };\n}\n\npub fn run() -> bool {\n    catalog_items!();\n    check(3)\n}\n";
    let type_catalog = "use crate::source::SCHEMA_VERSION;\n\npub fn check(schema: i32) -> bool {\n    schema > SCHEMA_VERSION\n}\n";
    let catalog_lines = vec![line_of(CATALOG, "if schema"), line_of(CATALOG, "format!(")];
    let report_line = |source: &str| ("src/report.rs", vec![line_of(source, "format!(")]);
    let cases = [
        // 変更した定数を本当に使う暗黙キャプチャは、マクロの有無を問わず残る。
        Case {
            name: "capture in the private subtree",
            store: &child_store,
            files: vec![("src/store/child.rs", child)],
            kept: vec![("src/store/child.rs", vec![line_of(child, "format!(")])],
            excluded: vec!["src/source.rs", "src/catalog.rs"],
        },
        Case {
            name: "capture in the private subtree without body macros",
            store: &plain_child_store,
            files: vec![("src/store/child.rs", child)],
            kept: vec![("src/store/child.rs", vec![line_of(child, "format!(")])],
            excluded: vec!["src/source.rs", "src/catalog.rs"],
        },
        // 暗黙キャプチャの束縛を証明できない位置。
        Case {
            name: "statement macro before the capture",
            store: &params,
            files: vec![
                ("src/lib.rs", with_report),
                ("src/report.rs", statement_macro),
            ],
            kept: vec![report_line(statement_macro)],
            excluded: vec!["src/source.rs", "src/catalog.rs"],
        },
        Case {
            name: "shadowed format macro",
            store: &params,
            files: vec![
                ("src/lib.rs", with_report),
                ("src/report.rs", shadowed_format),
            ],
            kept: vec![report_line(shadowed_format)],
            excluded: vec!["src/source.rs", "src/catalog.rs"],
        },
        Case {
            name: "format inside another macro",
            store: &params,
            files: vec![
                ("src/lib.rs", with_report),
                ("src/report.rs", nested_format),
            ],
            kept: vec![report_line(nested_format)],
            excluded: vec!["src/source.rs", "src/catalog.rs"],
        },
        Case {
            name: "format written as a path",
            store: &params,
            files: vec![("src/lib.rs", with_report), ("src/report.rs", std_path)],
            kept: vec![report_line(std_path)],
            excluded: vec!["src/source.rs", "src/catalog.rs"],
        },
        // crate 起点の import を証明できない環境 (直下の宣言の証明は環境に依らない)。
        Case {
            name: "module-level macro elsewhere",
            store: &params,
            files: vec![
                (
                    "src/lib.rs",
                    "pub mod catalog;\npub mod source;\npub mod store;\npub mod util;\n",
                ),
                ("src/util.rs", "make_items!();\n"),
            ],
            kept: vec![("src/catalog.rs", catalog_lines.clone())],
            excluded: vec!["src/source.rs"],
        },
        Case {
            name: "path attribute in a binary",
            store: &params,
            files: vec![(
                "src/bin/tool.rs",
                "#[path = \"../store.rs\"]\nmod source;\n\nfn main() {}\n",
            )],
            kept: vec![("src/catalog.rs", catalog_lines.clone())],
            excluded: vec!["src/source.rs"],
        },
        Case {
            name: "derive name replaced by an import",
            store: &params,
            files: vec![(
                "tests/relocate.rs",
                "use external_macros::Relocate as Clone;\n\n#[derive(Clone)]\nstruct Trigger;\n",
            )],
            kept: vec![("src/catalog.rs", catalog_lines.clone())],
            excluded: vec!["src/source.rs"],
        },
        // import の先に、参照と同じ名前空間の無条件の宣言が無い。
        Case {
            name: "import target declares only a type",
            store: inclusion_store,
            files: vec![
                (
                    "src/lib.rs",
                    "pub mod source;\npub mod store;\n#[cfg(any())]\npub mod catalog;\n",
                ),
                ("src/source.rs", "pub type SCHEMA_VERSION = ();\n"),
                ("src/catalog.rs", type_catalog),
            ],
            kept: vec![("src/catalog.rs", vec![line_of(type_catalog, "schema >")])],
            excluded: vec![],
        },
        Case {
            name: "conditional value declaration in the import target",
            store: &params,
            files: vec![(
                "src/source.rs",
                "#[cfg(any())]\npub const SCHEMA_VERSION: u32 = 2;\npub type SCHEMA_VERSION = ();\n",
            )],
            kept: vec![("src/catalog.rs", catalog_lines.clone())],
            excluded: vec![],
        },
    ];
    for case in cases {
        let repo = constants_repo(case.store, &case.files);
        let context = repo.run_json("context", &["--git"]);
        for (path, lines) in &case.kept {
            let found = caller_lines(&context, Some("impacted_callers"), path);
            for line in lines {
                assert!(
                    found.contains(line),
                    "{}: {path}:{line}: {context}",
                    case.name
                );
            }
        }
        for path in &case.excluded {
            assert!(
                caller_lines(&context, None, path).is_empty(),
                "{}: {path}: {context}",
                case.name
            );
        }
    }

    // 標準の derive と `#[test]` だけなら、import の証明は使える (上の差し替えの対照)。
    let repo = constants_repo(
        &params,
        &[(
            "tests/plain.rs",
            "#[derive(Clone, Debug)]\nstruct Trigger;\n\n#[test]\nfn works() {}\n",
        )],
    );
    let context = repo.run_json("context", &["--git"]);
    for path in ["src/source.rs", "src/catalog.rs"] {
        assert!(
            caller_lines(&context, None, path).is_empty(),
            "{path}: {context}"
        );
    }
}
