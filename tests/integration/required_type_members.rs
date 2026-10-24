//! TypeScript の interface へ必須プロパティを足し、値を作る箇所を同じ差分で直したときの
//! `review --git --hook` (Issue #89)。

use super::support::{TestRepo, cargo_bin};

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
