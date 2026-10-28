//! map で作る配列と consumer の同一差分内の追随を確認する。

use super::support::{TestRepo, cargo_bin};

fn array_removal_fixture(
    extension: &str,
    return_type: &str,
    producer: &str,
    usage: &str,
) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let asynchronous = if return_type.starts_with("Promise<") {
        "async "
    } else {
        ""
    };
    let row = format!(
        "export interface Row {{ id: string; note: string; extra: number; }}\nexport {asynchronous}function toRows(ids: string[]): {return_type} {{\n    return {producer};\n}}\n"
    );
    let output = if usage.contains("await") {
        format!("export async function labels(): Promise<string[]> {{\n    return {usage};\n}}\n")
    } else {
        format!("export const labels = {usage};\n")
    };
    let view = format!(
        "import {{ type Row, toRows }} from './row';\nexport function label(row: Row): string {{\n    return `${{row.id}} (${{row.note}}, ${{row.extra}})`;\n}}\n{output}"
    );
    let text = |source: String| {
        if extension == "tsx" {
            format!("// 表示用の行\r\n{}", source.replace('\n', "\r\n"))
        } else {
            source
        }
    };
    let row = text(row);
    let view = text(view);
    let row_path = format!("src/row.{extension}");
    let view_path = format!("src/view.{extension}");
    repo.write(&row_path, &row);
    repo.write(&view_path, &view);
    repo.commit_all("initial");
    let refs_before = (extension == "tsx" && return_type == "Row[]" && usage == MAP_LABELS)
        .then(|| repo.run_json("refs", &["--name", "toRows"]));
    repo.write(
        &row_path,
        row.replace(" extra: number;", "").replace(", extra: 0", ""),
    );
    repo.write(&view_path, view.replace(", ${row.extra}", ""));
    if let Some(before) = refs_before {
        assert_eq!(repo.run_json("refs", &["--name", "toRows"]), before);
    }
    repo
}

fn array_removal_hook(repo: &TestRepo, staged: bool) -> (i32, serde_json::Value) {
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--format", "json", "--dir"])
        .arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stderr).unwrap(),
    )
}

fn assert_array_removal_category(code: i32, json: &serde_json::Value, expected: i32, case: &str) {
    assert_eq!(code, expected, "{case}: {json}");
    let bucket = if expected == 0 { "mod_closed" } else { "mod" };
    assert!(
        json["api"][bucket]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == "Row")),
        "{case}: {json}"
    );
}

fn rewrite_array_removal(repo: &TestRepo, path: &str, old: &str, new: &str) {
    let source = std::fs::read_to_string(repo.root().join(path)).unwrap();
    assert!(source.contains(old), "missing fixture text {old}");
    repo.write(path, source.replace(old, new));
}

const MAP_ROWS: &str = "ids.map((id) => ({ id, note: id, extra: 0 }))";
const MAP_LABELS: &str = "toRows(['a']).map(label)";

#[test]
fn array_property_removal_closes_direct_map() {
    for (extension, annotation) in [
        ("ts", "Row[]"),
        ("tsx", "Row[]"),
        ("ts", "Array<Row>"),
        ("ts", "readonly Row[]"),
        ("ts", "Promise<Row[]>"),
        ("tsx", "Promise<Row[]>"),
    ] {
        let usage = if annotation.starts_with("Promise") {
            "(await toRows(['a'])).map(label)"
        } else {
            MAP_LABELS
        };
        let repo = array_removal_fixture(extension, annotation, MAP_ROWS, usage);
        let (code, json) = array_removal_hook(&repo, false);
        assert_array_removal_category(code, &json, 0, annotation);
        if annotation == "Row[]" {
            let normal = repo.run_json("review", &["--git"]);
            assert!(
                normal["api_changes"]["modified_closed_in_diff"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["name"] == "Row"),
                "{normal}"
            );
        }
        repo.stage_all();
        repo.write(
            format!("src/row.{extension}"),
            "export const worktreeOnly = 1;\n",
        );
        let (code, staged) = array_removal_hook(&repo, true);
        assert_array_removal_category(code, &staged, 0, annotation);
    }
}

#[test]
fn array_property_removal_keeps_unproven_producers_blocking() {
    for producer in [
        "ids.map((id) => { return { id, note: id, extra: 0 }; })",
        "ids.map?.((id) => ({ id, note: id, extra: 0 }))",
        "ids.filter(Boolean).map((id) => ({ id, note: id, extra: 0 }))",
        "ids.map((id) => ({ id, note: id, extra: 0 })).slice()",
        "ids.map(async (id) => ({ id, note: id, extra: 0 }))",
        "ids.map((id) => ({ ...{ id }, note: id, extra: 0 }))",
        "ids.map((id) => ({ id, ['note']: id, extra: 0 }))",
    ] {
        let repo = array_removal_fixture("ts", "Row[]", producer, MAP_LABELS);
        let (code, json) = array_removal_hook(&repo, false);
        assert_array_removal_category(code, &json, 1, producer);
    }
    for (path, old, new) in [
        ("src/row.ts", "note: id", "note: id, extra: 0"),
        ("src/view.ts", "${row.note}", "${row.note}, ${row.extra}"),
        ("src/row.ts", "ids: string[]", "ids: any[]"),
    ] {
        let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, MAP_LABELS);
        rewrite_array_removal(&repo, path, old, new);
        let (code, json) = array_removal_hook(&repo, false);
        assert_array_removal_category(code, &json, 1, new);
    }
}

#[test]
fn array_property_removal_keeps_unproven_consumers_and_escapes_blocking() {
    for usage in [
        "toRows(['a']).map(label, undefined)",
        "toRows(['a']).map?.(label)",
        "toRows(['a']).map<string>(label)",
        "toRows(['a'])?.map(label)",
        "toRows?.(['a']).map(label)",
        "toRows(['a']).forEach(label)",
        "toRows(['a']).map((row) => label(row))",
        "(() => { const rows = toRows(['a']); return rows.map(label); })()",
    ] {
        let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, usage);
        let (code, json) = array_removal_hook(&repo, false);
        assert_array_removal_category(code, &json, 1, usage);
    }
    for (old, new) in [
        ("row: Row): string", "row: Row): unknown"),
        ("row: Row): string", "row: Row, index?: number): string"),
        ("return `${row.id} (${row.note})`;", "return row;"),
    ] {
        let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, MAP_LABELS);
        rewrite_array_removal(&repo, "src/view.ts", old, new);
        let (code, json) = array_removal_hook(&repo, false);
        assert_array_removal_category(code, &json, 1, new);
    }
    let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, MAP_LABELS);
    repo.write("src/patch.ts", "Array.prototype.map = replacement;\n");
    let (code, json) = array_removal_hook(&repo, false);
    assert_array_removal_category(code, &json, 1, "prototype replacement");
}

#[test]
fn array_property_removal_requires_direct_await_and_scalar_removal_mode() {
    for (annotation, usage) in [
        (
            "Promise<Row[]>",
            "toRows(['a']).then((rows) => rows.map(label))",
        ),
        ("Promise<Row[]>", "(await toRows(['a']) ?? []).map(label)"),
        ("Row[]", "(await toRows(['a'])).map(label)"),
    ] {
        let repo = array_removal_fixture("ts", annotation, MAP_ROWS, usage);
        let (code, json) = array_removal_hook(&repo, false);
        assert_array_removal_category(code, &json, 1, usage);
    }
    let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, MAP_LABELS);
    rewrite_array_removal(&repo, "src/row.ts", "note: string;", "note: string[];");
    rewrite_array_removal(&repo, "src/row.ts", "note: id", "note: []");
    let (code, json) = array_removal_hook(&repo, false);
    assert_array_removal_category(code, &json, 1, "non-scalar invariant stays separate");
}

#[test]
fn array_property_removal_does_not_extend_narrowing_or_return_row_consumers() {
    let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, MAP_LABELS);
    rewrite_array_removal(&repo, "src/view.ts", "row: Row): string", "row: Row): Row");
    rewrite_array_removal(
        &repo,
        "src/view.ts",
        "return `${row.id} (${row.note})`;",
        "return row;",
    );
    let (code, json) = array_removal_hook(&repo, false);
    assert_array_removal_category(code, &json, 1, "consumer returns Row");

    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let source = "export interface Row { id: string; note: string | null }\nexport function toRows(ids: string[]): Row[] {\n    return ids.map((_id) => ({ id: 'fixed', note: null }));\n}\n";
    repo.write("src/row.ts", source);
    repo.commit_all("initial");
    repo.write(
        "src/row.ts",
        source
            .replace("note: string | null", "note: string")
            .replace("note: null", "note: 'ready'"),
    );
    let (code, json) = array_removal_hook(&repo, false);
    assert_array_removal_category(code, &json, 1, "narrowing stays outside array proof");
}

#[test]
fn array_property_removal_checks_alias_shadow_and_negative_index_snapshot() {
    let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, MAP_LABELS);
    rewrite_array_removal(
        &repo,
        "src/view.ts",
        "type Row, toRows }",
        "type Row, toRows as makeRows }",
    );
    rewrite_array_removal(&repo, "src/view.ts", "toRows(['a'])", "makeRows(['a'])");
    let (code, json) = array_removal_hook(&repo, false);
    assert_array_removal_category(code, &json, 1, "producer import alias");
    for usage in [
        "((label: (value: unknown) => string) => toRows(['a']).map(label))(() => 'local')",
        "(() => { function toRows() { return []; } return toRows().map(label); })()",
    ] {
        let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, usage);
        let (code, json) = array_removal_hook(&repo, false);
        assert_array_removal_category(code, &json, 1, usage);
    }
    let repo = array_removal_fixture("ts", "Row[]", MAP_ROWS, MAP_LABELS);
    rewrite_array_removal(
        &repo,
        "src/view.ts",
        "${row.note}",
        "${row.note}, ${row.extra}",
    );
    repo.stage_all();
    rewrite_array_removal(&repo, "src/view.ts", ", ${row.extra}", "");
    let (code, staged) = array_removal_hook(&repo, true);
    assert_array_removal_category(code, &staged, 1, "index retains deleted reader");
    let (code, working) = array_removal_hook(&repo, false);
    assert_array_removal_category(code, &working, 0, "working tree fixed reader");
}
