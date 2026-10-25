//! TypeScript の interface へ必須プロパティを足し、値を作る箇所を同じ差分で直したときの
//! `review --git --hook` (Issue #89)。

use super::support::{TestRepo, cargo_bin};

#[test]
fn typed_spread_copy_matches_normal_hook_and_index_without_changing_refs() {
    for extension in ["ts", "tsx"] {
        for (copy, call, expected) in [
            (
                "export function copy(row: Row, id: string): Row { return { ...row, id }; }",
                "copy(toRow('a'), 'b')",
                0,
            ),
            (
                "export function copy(): Row { return { ...toRow('a'), id: 'b' }; }",
                "copy()",
                0,
            ),
            (
                "export function copy(): Row { const row: Row = toRow('a'); return { ...row, id: 'b' }; }",
                "copy()",
                0,
            ),
            (
                "export function copy(row: any): Row { return { ...row, id: 'b' }; }",
                "copy(toRow('a'))",
                1,
            ),
            (
                "export function copy(row: Row): Row { return { ...row, note: 123 }; }",
                "copy(toRow('a'))",
                1,
            ),
        ] {
            let repo = TestRepo::new();
            repo.init_git();
            repo.create_dir_all("src");
            let row_path = format!("src/row.{extension}");
            let copy_path = format!("src/copy.{extension}");
            repo.write(&row_path, ROW_BASE);
            repo.write(
                &copy_path,
                format!("import {{ type Row, toRow }} from './row';\n{copy}\n"),
            );
            repo.write("src/app.ts", format!("import {{ toRow }} from './row';\nimport {{ copy }} from './copy';\nexport const result = {call}.id;\n"));
            repo.commit_all("initial");
            let copy_refs = || {
                let output = cargo_bin()
                    .args(["refs", "--name", "Row", "--format", "json", "--dir"])
                    .arg(repo.root())
                    .output()
                    .unwrap();
                let refs: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                refs["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|r| r["path"] == copy_path)
                    .cloned()
                    .collect::<Vec<_>>()
            };
            let before = copy_refs();
            repo.write(
                &row_path,
                ROW_BASE
                    .replace("\tid: string;", "\tid: string;\n\tnote: string | null;")
                    .replace("return { id };", "return { id, note: null };"),
            );
            let (code, json) = hook(&repo);
            let bucket = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{extension}/{copy}: {json}");
            assert_eq!(json["api"][bucket][0]["n"], "Row", "{json}");
            let output = cargo_bin()
                .args(["review", "--git", "--format", "json", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            let normal: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let bucket = if expected == 0 {
                "modified_closed_in_diff"
            } else {
                "modified"
            };
            assert_eq!(normal["api_changes"][bucket][0]["name"], "Row", "{normal}");
            assert!(!before.is_empty());
            assert_eq!(copy_refs(), before);
            repo.stage_all();
            repo.write(&copy_path, "import type { Row } from './row';\nexport function copy(row: Row): Row { return { id: row.id }; }\n");
            let output = cargo_bin()
                .args(["review", "--git", "--staged", "--hook", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            let staged: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(output.status.code(), Some(expected), "{staged}");
            let bucket = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(staged["api"][bucket][0]["n"], "Row", "{staged}");
        }
    }
}

const ROW_BASE: &str = "export interface Row {\n\tid: string;\n}\n\nexport function toRow(id: string): Row {\n\treturn { id };\n}\n";
const VIEW_BASE: &str = "import { type Row, toRow } from \"./row\";\n\nexport function label(row: Row): string {\n\treturn row.id;\n}\n\nexport const first = label(toRow(\"a\"));\n";
const VIEW_AFTER: &str = "import { type Row, toRow } from \"./row\";\n\nexport function label(row: Row): string {\n\treturn row.note === null ? row.id : `${row.id} (${row.note})`;\n}\n\nexport const first = label(toRow(\"a\"));\n";

fn hook(repo: &TestRepo) -> (i32, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let json = serde_json::from_slice(&output.stderr).unwrap();
    (output.status.code().unwrap(), json)
}

fn fixture(to_row_body: &str, extra: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/row.ts", ROW_BASE);
    repo.write("src/view.ts", VIEW_BASE);
    for (path, content) in extra {
        repo.write(path, content);
    }
    repo.commit_all("initial");
    repo.write(
        "src/row.ts",
        format!(
            "export interface Row {{\n\tid: string;\n\tnote: string | null;\n}}\n\nexport function toRow(id: string): Row {{\n\treturn {to_row_body};\n}}\n"
        ),
    );
    repo.write("src/view.ts", VIEW_AFTER);
    repo
}

#[test]
fn producer_updated_in_same_diff_closes_required_member_addition() {
    let (code, json) = hook(&fixture("{ id, note: null }", &[]));
    assert_eq!(json["api"]["mod_closed"][0]["n"], "Row", "{json}");
    assert!(json["api"]["mod"].is_null(), "{json}");
    assert_eq!(code, 0, "{json}");
}

#[test]
fn local_producer_binding_hook_matches_worktree_and_index() {
    for extension in ["ts", "tsx"] {
        for (extra, expected) in [
            ("", 0),
            ("type Copy = typeof row;", 1),
            ("{ const row = { id: 'stale' }; label(row); }", 1),
            ("{ const row = 1; type Copy = typeof row; }", 0),
        ] {
            let app = format!(
                "import {{ type Row, toRow }} from './row';\nimport {{ label }} from './view';\nexport function run(): string {{ const row: Row = toRow('a'); {extra} return label(row); }}\n"
            );
            let path = format!("src/app.{extension}");
            let repo = fixture("{ id, note: null }", &[(&path, &app)]);
            let (code, json) = hook(&repo);
            let category = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{extension}/{extra}: {json}");
            assert_eq!(json["api"][category][0]["n"], "Row", "{json}");
            repo.stage_all();
            repo.write(&path, "export const unrelated = 1;\n");
            let output = cargo_bin()
                .args(["review", "--git", "--staged", "--hook", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            let staged: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(output.status.code(), Some(expected), "{staged}");
            assert_eq!(staged["api"][category][0]["n"], "Row", "{staged}");
        }
    }
}

#[test]
fn inferred_const_call_result_matches_normal_hook_and_staged_review() {
    for extension in ["ts", "tsx"] {
        for (body, expected) in [
            ("return label(toRow('a'));", 0),
            ("const row: Row = toRow('a'); return label(row);", 0),
            ("const row = toRow('a'); return label(row);", 0),
            (
                "const row = toRow('a'); type Copy = typeof row; return label(row);",
                1,
            ),
            (
                "const row = toRow('a'); const other = row; return label(row);",
                1,
            ),
            ("let row = toRow('a'); return label(row);", 1),
        ] {
            let app = format!(
                "import {{ type Row, toRow }} from './row';\nimport {{ label }} from './view';\nexport function run(): string {{ {body} }}\n"
            );
            let path = format!("src/app.{extension}");
            let repo = fixture("{ id, note: null }", &[(&path, &app)]);
            let (code, json) = hook(&repo);
            let category = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{extension}/{body}: {json}");
            assert_eq!(json["api"][category][0]["n"], "Row", "{json}");
            let output = cargo_bin()
                .args(["review", "--git", "--format", "json", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            let normal: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let category = if expected == 0 {
                "modified_closed_in_diff"
            } else {
                "modified"
            };
            assert_eq!(
                normal["api_changes"][category][0]["name"], "Row",
                "{normal}"
            );
            repo.stage_all();
            repo.write(&path, "export const unrelated = 1;\n");
            let output = cargo_bin()
                .args(["review", "--git", "--staged", "--hook", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            let staged: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(output.status.code(), Some(expected), "{staged}");
            let category = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(staged["api"][category][0]["n"], "Row", "{staged}");
        }
    }
}

#[test]
fn producer_left_unchanged_keeps_required_member_addition_blocking() {
    // toRow が note を返さない (型検査も通らない) 差分。
    let (code, json) = hook(&fixture("{ id }", &[]));
    assert_eq!(json["api"]["mod"][0]["n"], "Row", "{json}");
    assert_eq!(code, 1, "{json}");

    // 値を作る箇所が別ファイルにあり、そのファイルを直していない。
    let sample =
        "import type { Row } from \"./row\";\n\nexport const SAMPLE: Row = { id: \"s\" };\n";
    let (code, json) = hook(&fixture("{ id, note: null }", &[("src/sample.ts", sample)]));
    assert_eq!(json["api"]["mod"][0]["n"], "Row", "{json}");
    assert_eq!(code, 1, "{json}");
}

#[test]
fn map_producer_required_member_addition_closes_only_updated_values() {
    for (body, expected) in [
        ("return ids.map((id) => ({ id, note: null }));", 0),
        ("return ids.map((id) => ({ id }));", 1),
        (
            "if (ids.length) return [] as Row[]; return ids.map((id) => ({ id, note: null }));",
            1,
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write("src/row.ts", "export interface Row {\n    id: string;\n}\nexport function toRows(ids: string[]): Row[] {\n    return ids.map((id) => ({ id }));\n}\n");
        repo.write("src/view.ts", "import { toRows } from './row';\nexport const ids = toRows(['a', 'b']).map((row) => row.id);\n");
        repo.commit_all("initial");
        repo.write("src/row.ts", format!("export interface Row {{\n    id: string;\n    note: string | null;\n}}\nexport function toRows(ids: string[]): Row[] {{\n    {body}\n}}\n"));
        let (code, json) = hook(&repo);
        assert_eq!(code, expected, "{body}: {json}");
        let category = if expected == 0 { "mod_closed" } else { "mod" };
        assert_eq!(json["api"][category][0]["n"], "Row", "{body}: {json}");
        // 作業ツリーと異なる index でも、同じ staged ソースで証明する。
        repo.stage_all();
        repo.write("src/row.ts", "export const unrelated = 1;\n");
        let output = cargo_bin()
            .args(["review", "--git", "--staged", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        let staged: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(expected), "{body}: {staged}");
        assert_eq!(staged["api"][category][0]["n"], "Row", "{body}: {staged}");
    }
}

#[test]
fn member_type_narrowing_hook_checks_producers_and_uses_in_worktree_and_index() {
    let base = "export interface Row {\n    id: string;\n    note: string | null;\n}\nexport function toRow(id: string): Row {\n    return { id, note: null };\n}\n";
    let view_base = "import { type Row, toRow } from './row';\nexport function label(row: Row): string {\n    return row.note === null ? row.id : `${row.id} (${row.note})`;\n}\nexport const first = label(toRow('a'));\n";
    for (value, stale_comparison, expected) in
        [("id", false, 0), ("null", false, 1), ("id", true, 1)]
    {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write("src/row.ts", base);
        repo.write("src/view.ts", view_base);
        repo.commit_all("initial");
        repo.write(
            "src/row.ts",
            base.replace("note: string | null", "note: string")
                .replace("note: null", &format!("note: {value}")),
        );
        let view = if stale_comparison {
            view_base.to_string()
        } else {
            view_base.replace("row.note === null ? row.id : ", "")
        };
        repo.write("src/view.ts", view);
        let (code, json) = hook(&repo);
        let category = if expected == 0 { "mod_closed" } else { "mod" };
        assert_eq!(code, expected, "{json}");
        assert_eq!(json["api"][category][0]["n"], "Row", "{json}");
        repo.stage_all();
        repo.write("src/row.ts", "export const unrelated = 1;\n");
        let output = cargo_bin()
            .args(["review", "--git", "--staged", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        let staged: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(expected), "{staged}");
        assert_eq!(staged["api"][category][0]["n"], "Row", "{staged}");
    }
}

#[test]
fn member_property_removal_hook_checks_values_uses_and_index_snapshot() {
    let row_base = "export interface Row {\n    id: string;\n    note: string;\n    extra: number;\n}\nexport function toRow(id: string): Row {\n    return { id, note: id, extra: 0 };\n}\n";
    let view_base = "import { type Row, toRow } from './row';\nexport function label(row: Row): string {\n    return `${row.id} (${row.note}, ${row.extra})`;\n}\nexport const first = label(toRow('a'));\n";
    for alias in [false, true] {
        for case in ["updated", "producer", "reader", "dynamic key"] {
            let as_alias = |source: &str| {
                if alias {
                    source
                        .replace("export interface Row {", "export type Row = {")
                        .replace("\n}\nexport function", "\n};\nexport function")
                } else {
                    source.to_owned()
                }
            };
            let repo = TestRepo::new();
            repo.init_git();
            repo.create_dir_all("src");
            repo.write("src/row.ts", as_alias(row_base));
            repo.write("src/view.ts", view_base);
            repo.commit_all("initial");
            let dead_names = |json: serde_json::Value| -> Vec<_> {
                json["dead_symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|symbol| {
                        (
                            symbol["name"].clone(),
                            symbol["kind"].clone(),
                            symbol["file"].clone(),
                        )
                    })
                    .collect()
            };
            let baseline_dead = if case == "updated" {
                Some(dead_names(repo.run_json("dead-code", &[])))
            } else {
                None
            };
            let mut row = row_base.replace("    extra: number;\n", "");
            if case != "producer" {
                row = row.replace(", extra: 0", "");
            }
            let view = match case {
                "reader" => view_base.to_owned(),
                "dynamic key" => view_base.replace(
                    "return `${row.id} (${row.note}, ${row.extra})`;",
                    "const key = 'extra'; return `${row[key]}`;",
                ),
                _ => view_base.replace(", ${row.extra}", ""),
            };
            repo.write("src/row.ts", as_alias(&row));
            repo.write("src/view.ts", &view);
            let expected = i32::from(case != "updated");
            let category = if expected == 0 { "mod_closed" } else { "mod" };
            let (code, json) = hook(&repo);
            assert_eq!(code, expected, "alias={alias}, {case}: {json}");
            assert_eq!(json["api"][category][0]["n"], "Row", "{json}");
            if let Some(baseline) = baseline_dead {
                assert_eq!(dead_names(repo.run_json("dead-code", &[])), baseline);
                let refs = repo.run_json("refs", &["--name", "extra"]);
                assert!(refs["refs"].as_array().unwrap().is_empty(), "{refs}");
            }
            if case == "updated" {
                let normal = repo.run_json("review", &["--git"]);
                assert_eq!(
                    normal["api_changes"]["modified_closed_in_diff"][0]["name"], "Row",
                    "{normal}"
                );
            }
            repo.stage_all();
            repo.write("src/row.ts", "export const unrelated = 1;\n");
            repo.write("src/view.ts", "export const unrelated = 2;\n");
            let output = cargo_bin()
                .args(["review", "--git", "--staged", "--hook", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            let staged: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(output.status.code(), Some(expected), "{staged}");
            assert_eq!(staged["api"][category][0]["n"], "Row", "{staged}");
        }
    }
}
