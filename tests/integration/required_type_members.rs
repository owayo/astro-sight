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
