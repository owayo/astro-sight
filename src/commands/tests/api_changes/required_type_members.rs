//! TypeScript の interface / object 型の type alias へ必須プロパティを足した変更の閉包判定
//! (Issue #89)。値を作る箇所と受け取る関数の呼び出し側が同一 diff で追随していれば
//! `modified_closed_in_diff`、1 つでも証明できなければ blocking な `modified` に残す。
//!
//! 各ケースの変更後のソースは、tsc 5.9 (strict) で「closed」のものが型検査を通り、
//! 「blocking」のうち型エラーになるもの (未更新の箇所が残るもの) が実際に落ちることを
//! 確かめてある。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

const ROW_BASE: &str = r#"export interface Row {
    id: string;
}

export function toRow(id: string): Row {
    return { id };
}
"#;

const ROW_AFTER: &str = r#"export interface Row {
    id: string;
    note: string | null;
}

export function toRow(id: string): Row {
    return { id, note: null };
}
"#;

const VIEW_BASE: &str = r#"import { type Row, toRow } from "./row";

export function label(row: Row): string {
    return row.id;
}

export const first = label(toRow("a"));
"#;

const VIEW_AFTER: &str = r#"import { type Row, toRow } from "./row";

export function label(row: Row): string {
    return row.note === null ? row.id : `${row.id} (${row.note})`;
}

export const first = label(toRow("a"));
"#;

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Closed,
    Blocking,
    Absent,
}

/// base を commit し、`after` で作業ツリーを書き換えて `review --git` と同じ経路で API 差分を出す。
fn api_after(base: &[(&str, &str)], after: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, base, "base");
    for (path, content) in after {
        let full = repo.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(full, content).expect("write");
    }
    detect_api_changes_from_worktree(repo)
}

fn verdict(api: &ApiChanges, name: &str) -> Verdict {
    if api.modified_closed_in_diff.iter().any(|m| m.name == name) {
        Verdict::Closed
    } else if api.modified.iter().any(|m| m.name == name) {
        Verdict::Blocking
    } else {
        Verdict::Absent
    }
}

/// Issue の例 (row.ts と view.ts だけ) に `extra` のファイルを足した base と変更後。
fn row_verdict(
    extra_base: &[(&str, &str)],
    row_after: &str,
    extra_after: &[(&str, &str)],
) -> Verdict {
    let mut base = vec![("src/row.ts", ROW_BASE), ("src/view.ts", VIEW_BASE)];
    base.extend_from_slice(extra_base);
    let mut after = vec![("src/row.ts", row_after), ("src/view.ts", VIEW_AFTER)];
    after.extend_from_slice(extra_after);
    verdict(&api_after(&base, &after), "Row")
}

#[test]
fn required_member_closes_when_producers_and_consumer_callers_are_updated() {
    // Issue の例: 値を作る toRow を直し、受け取る label の呼び出しは toRow の結果を渡す。
    assert_eq!(row_verdict(&[], ROW_AFTER, &[]), Verdict::Closed);

    // toRow を直さない差分は型検査も通らない。
    let stale = ROW_AFTER.replace("{ id, note: null }", "{ id }");
    assert_eq!(row_verdict(&[], &stale, &[]), Verdict::Blocking);

    // object 型の type alias も同じ扱い。
    let alias = |source: &str| {
        source
            .replace("export interface Row {", "export type Row = {")
            .replace("\n}\n\nexport function", "\n};\n\nexport function")
    };
    let (alias_base, alias_after) = (alias(ROW_BASE), alias(ROW_AFTER));
    let api = api_after(
        &[("src/row.ts", &alias_base), ("src/view.ts", VIEW_BASE)],
        &[("src/row.ts", &alias_after), ("src/view.ts", VIEW_AFTER)],
    );
    assert_eq!(verdict(&api, "Row"), Verdict::Closed, "{api:?}");

    // `export { Row }` で公開するだけの参照は妨げにならない。
    let clause = |source: &str| {
        source.replacen("export interface Row", "interface Row", 1) + "export { Row };\n"
    };
    let (clause_base, clause_after) = (clause(ROW_BASE), clause(ROW_AFTER));
    let api = api_after(
        &[("src/row.ts", &clause_base), ("src/view.ts", VIEW_BASE)],
        &[("src/row.ts", &clause_after), ("src/view.ts", VIEW_AFTER)],
    );
    assert_eq!(verdict(&api, "Row"), Verdict::Closed, "{api:?}");
}

#[test]
fn required_member_value_type_must_fit_the_added_member() {
    for (value, expected) in [
        ("null", Verdict::Closed),
        ("\"memo\"", Verdict::Closed),
        ("`memo`", Verdict::Closed),
        // 型が合わない値はキーがあっても型検査で落ちる。
        ("123", Verdict::Blocking),
        ("undefined", Verdict::Blocking),
        // 値の型を確かめられない式は証明しない。
        ("String(id)", Verdict::Blocking),
        // escape を含むキー (`n\u006fte` は `note` と同じ名前) は、名前を比べられない。
        ("null, n\\u006fte: 123", Verdict::Blocking),
        ("null, \"n\\u006fte\": 123", Verdict::Blocking),
    ] {
        let after = ROW_AFTER.replace("note: null", &format!("note: {value}"));
        assert_eq!(row_verdict(&[], &after, &[]), expected, "note: {value}");
    }

    // 型注釈の付いた引数の値 (shorthand) は注釈の型で確かめる。
    let base = ROW_BASE.replace(
        "toRow(id: string)",
        "toRow(id: string, note: string | null)",
    );
    let after = ROW_AFTER
        .replace(
            "toRow(id: string)",
            "toRow(id: string, note: string | null)",
        )
        .replace("note: null }", "note }");
    let view = |source: &str| source.replace("toRow(\"a\")", "toRow(\"a\", null)");
    let (view_base, view_after) = (view(VIEW_BASE), view(VIEW_AFTER));
    let api = api_after(
        &[("src/row.ts", &base), ("src/view.ts", &view_base)],
        &[("src/row.ts", &after), ("src/view.ts", &view_after)],
    );
    assert_eq!(verdict(&api, "Row"), Verdict::Closed, "{api:?}");
}

#[test]
fn required_member_requires_every_producer_to_be_updated() {
    let two_returns = |empty: &str, rest: &str| {
        format!(
            "export interface Row {{\n    id: string;\n    note: string | null;\n}}\n\nexport function toRow(id: string): Row {{\n    if (id === \"\") {{\n        return {empty};\n    }}\n    return {rest};\n}}\n"
        )
    };
    let base = "export interface Row {\n    id: string;\n}\n\nexport function toRow(id: string): Row {\n    if (id === \"\") {\n        return { id: \"empty\" };\n    }\n    return { id };\n}\n";
    let all = two_returns("{ id: \"empty\", note: \"blank\" }", "{ id, note: null }");
    let one = two_returns("{ id: \"empty\" }", "{ id, note: null }");
    for (after, expected) in [(all, Verdict::Closed), (one, Verdict::Blocking)] {
        let api = api_after(
            &[("src/row.ts", base), ("src/view.ts", VIEW_BASE)],
            &[("src/row.ts", &after), ("src/view.ts", VIEW_AFTER)],
        );
        assert_eq!(verdict(&api, "Row"), expected, "{after}");
    }

    // 別ファイルの `const x: Row = ...` と `satisfies Row` も値を作る箇所。
    let sample_base = "import type { Row } from \"./row\";\n\nexport const SAMPLE: Row = { id: \"s\" };\nexport const OTHER = { id: \"o\" } satisfies Row;\n";
    let sample_after = "import type { Row } from \"./row\";\n\nexport const SAMPLE: Row = { id: \"s\", note: \"n\" };\nexport const OTHER = {\n    id: \"o\",\n    note: null,\n} satisfies Row;\n";
    assert_eq!(
        row_verdict(
            &[("src/sample.ts", sample_base)],
            ROW_AFTER,
            &[("src/sample.ts", sample_after)]
        ),
        Verdict::Closed
    );
    // 別ファイルの値を作る箇所が差分の外に残る。
    assert_eq!(
        row_verdict(&[("src/sample.ts", sample_base)], ROW_AFTER, &[]),
        Verdict::Blocking
    );
    // import 型 (`import("./row").Row`) は型の位置でもメンバー式になる。値の名前空間の
    // 出現と取り違えて読み飛ばさない。
    let import_type = "export const SAMPLE: import(\"./row\").Row = { id: \"s\" };\n";
    assert_eq!(
        row_verdict(&[("src/sample.ts", import_type)], ROW_AFTER, &[]),
        Verdict::Blocking
    );
}

#[test]
fn required_member_consumer_callers_must_pass_proven_values() {
    let use_base =
        "import { label } from \"./view\";\n\nexport const second = label({ id: \"b\" });\n";
    for (use_after, expected) in [
        // 呼び出し側で直接渡す object literal を同じ差分で直した。
        (
            "import { label } from \"./view\";\n\nexport const second = label({ id: \"b\", note: \"x\" });\n",
            Verdict::Closed,
        ),
        // 型注釈がちょうど Row の const を渡す (const 自体も値を作る箇所として確かめる)。
        (
            "import type { Row } from \"./row\";\nimport { label } from \"./view\";\n\nconst SAMPLE: Row = { id: \"b\", note: null };\nexport const second = label(SAMPLE);\n",
            Verdict::Closed,
        ),
        // 呼び出し側を直していない。
        (use_base, Verdict::Blocking),
        // 型注釈の無い const は literal の widening があり、型を証明しない。
        (
            "import { label } from \"./view\";\n\nconst value = { id: \"b\", note: null };\nexport const second = label(value);\n",
            Verdict::Blocking,
        ),
    ] {
        assert_eq!(
            row_verdict(
                &[("src/use.ts", use_base)],
                ROW_AFTER,
                &[("src/use.ts", use_after)]
            ),
            expected,
            "{use_after}"
        );
    }

    // 型注釈が Row の引数をそのまま渡すのは Row 型の値。
    let twice = "\nexport function twice(row: Row): string {\n    return label(row) + label(row);\n}\n\nexport const pair = twice(toRow(\"b\"));\n";
    let (view_base, view_after) = (
        format!("{VIEW_BASE}{twice}"),
        format!("{VIEW_AFTER}{twice}"),
    );
    let api = api_after(
        &[("src/row.ts", ROW_BASE), ("src/view.ts", &view_base)],
        &[("src/row.ts", ROW_AFTER), ("src/view.ts", &view_after)],
    );
    assert_eq!(verdict(&api, "Row"), Verdict::Closed, "{api:?}");

    // 値として渡す (呼び出しが見えない)・barrel 経由で古い値を渡す呼び出しは証明しない。
    for (extra, expected) in [
        (
            vec![(
                "src/use.ts",
                "import { toRow } from \"./row\";\nimport { label } from \"./view\";\n\nexport const all = [toRow(\"x\")].map(label);\n",
            )],
            Verdict::Blocking,
        ),
        (
            vec![
                ("src/index.ts", "export { label } from \"./view\";\n"),
                (
                    "src/use.ts",
                    "import { label } from \"./index\";\n\nexport const second = label({ id: \"b\" });\n",
                ),
            ],
            Verdict::Blocking,
        ),
    ] {
        assert_eq!(row_verdict(&extra, ROW_AFTER, &[]), expected, "{extra:?}");
    }
}

#[test]
fn required_member_consumer_must_not_rebind_the_parameter() {
    let view = |body: &str| {
        format!(
            "import {{ type Row, toRow }} from \"./row\";\n\nexport function label(row: Row): string {{\n    {body}\n    return row.id;\n}}\n\nexport const first = label(toRow(\"a\"));\n"
        )
    };
    for (body, expected) in [
        // 引数への再代入は、変更後に型エラーになる。
        (
            "if (row.id === \"\") row = { id: \"fallback\" };",
            Verdict::Blocking,
        ),
        // プロパティへの代入は引数の束縛を変えない (対照)。
        ("row.id = row.id.trim();", Verdict::Closed),
        // 引数を別名へ束縛すると、その先の `typeof alias` は引数の名前の参照に現れない。
        (
            "const alias = row;\n    const fallback: typeof alias = { id: \"fallback\" };\n    console.log(fallback.id);",
            Verdict::Blocking,
        ),
        // メンバーの読み出しは引数の値を写さない (対照)。
        (
            "const name = row.id;\n    console.log(name);",
            Verdict::Closed,
        ),
    ] {
        let source = view(body);
        let api = api_after(
            &[("src/row.ts", ROW_BASE), ("src/view.ts", &source)],
            &[("src/row.ts", ROW_AFTER)],
        );
        assert_eq!(verdict(&api, "Row"), expected, "{body}");
    }
}

#[test]
fn required_member_typeof_of_a_producer_keeps_blocking() {
    // `ReturnType<typeof toRow>` は Row の名前を出さずに Row を取り出す。
    let derived = "import { toRow } from \"./row\";\n\ntype Derived = ReturnType<typeof toRow>;\nexport const derived: Derived = { id: \"d\" };\n";
    assert_eq!(
        row_verdict(&[("src/derived.ts", derived)], ROW_AFTER, &[]),
        Verdict::Blocking
    );

    // `satisfies Row` で作った値を束縛した変数と、括弧で包んだ arrow の束縛名も確かめる。
    let copy = "\ntype Copy = typeof sample;\nexport const copy: Copy = { id: \"b\" };\n";
    let sample_base = "import type { Row } from \"./row\";\n\nexport const sample = { id: \"a\" } satisfies Row;\n";
    let sample_after = "import type { Row } from \"./row\";\n\nexport const sample = { id: \"a\", note: \"memo\" } satisfies Row;\n";
    let arrow_base = "import type { Row } from \"./row\";\n\nexport const sample = ((): Row => ({ id: \"a\" }));\n";
    let arrow_after = "import type { Row } from \"./row\";\n\nexport const sample = ((): Row => ({ id: \"a\", note: null }));\n";
    let arrow_copy =
        "\ntype Copy = ReturnType<typeof sample>;\nexport const copy: Copy = { id: \"b\" };\n";
    for (base, after, tail) in [
        (sample_base, sample_after, copy),
        (arrow_base, arrow_after, arrow_copy),
    ] {
        let (with_base, with_after) = (format!("{base}{tail}"), format!("{after}{tail}"));
        assert_eq!(
            row_verdict(
                &[("src/sample.ts", &with_base)],
                ROW_AFTER,
                &[("src/sample.ts", &with_after)]
            ),
            Verdict::Blocking,
            "{with_after}"
        );
        // 対照: `typeof` が無ければ閉じる。
        assert_eq!(
            row_verdict(
                &[("src/sample.ts", base)],
                ROW_AFTER,
                &[("src/sample.ts", after)]
            ),
            Verdict::Closed,
            "{after}"
        );
    }

    // 別名の import・再 export の先の `typeof` は、元の名前の参照に現れない。
    for extra in [
        vec![(
            "src/copy.ts",
            "import { toRow as make } from \"./row\";\n\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"b\" };\n",
        )],
        vec![
            ("src/index.ts", "export { toRow as make } from \"./row\";\n"),
            (
                "src/copy.ts",
                "import { make } from \"./index\";\n\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"b\" };\n",
            ),
        ],
    ] {
        assert_eq!(
            row_verdict(&extra, ROW_AFTER, &[]),
            Verdict::Blocking,
            "{extra:?}"
        );
    }

    // 値を作る関数を別の名前へ束縛する・オブジェクトへ入れると、その先の `typeof` は元の名前の
    // 参照に現れない。
    for copy in [
        "import { toRow } from \"./row\";\n\nconst make = toRow;\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"b\" };\n",
        "import { toRow } from \"./row\";\n\nexport const registry = { toRow };\ntype Copy = ReturnType<(typeof registry)[\"toRow\"]>;\nexport const copy: Copy = { id: \"b\" };\n",
    ] {
        assert_eq!(
            row_verdict(&[("src/copy.ts", copy)], ROW_AFTER, &[]),
            Verdict::Blocking,
            "{copy}"
        );
    }
    // 条件式の分岐も値をそのまま伝える。
    let ternary = "import { toRow } from \"./row\";\n\ndeclare const flag: boolean;\nconst make = flag ? toRow : toRow;\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"old\" };\n";
    assert_eq!(
        row_verdict(&[("src/copy.ts", ternary)], ROW_AFTER, &[]),
        Verdict::Blocking
    );
    let branches = |note: &str, tail: &str| {
        format!(
            "import type {{ Row }} from \"./row\";\n\ndeclare const flag: boolean;\nexport const sample = flag ? ({{ id: \"a\"{note} }} satisfies Row) : ({{ id: \"b\"{note} }} satisfies Row);\n{tail}"
        )
    };
    let tail = "export const copy: typeof sample = { id: \"old\" };\n";
    for (tail, expected) in [(tail, Verdict::Blocking), ("", Verdict::Closed)] {
        let (base, after) = (branches("", tail), branches(", note: \"m\"", tail));
        assert_eq!(
            row_verdict(&[("src/s.ts", &base)], ROW_AFTER, &[("src/s.ts", &after)]),
            expected,
            "{after}"
        );
    }

    // 対照: コールバックとして渡すだけなら、名前を付け替えない (推論で型が伝わる範囲は対象外)。
    let callback = "import { toRow } from \"./row\";\n\nexport const ids = [\"a\", \"b\"].map(toRow).map((row) => row.id);\n";
    assert_eq!(
        row_verdict(&[("src/list.ts", callback)], ROW_AFTER, &[]),
        Verdict::Closed
    );

    // 値を入れた外側のオブジェクト・配列の `typeof` も Row を写す。
    for (base, after) in [
        (
            "export const box = { item: { id: \"a\" } satisfies Row };\ntype Copy = typeof box;\nexport const copy: Copy = { item: { id: \"b\" } };\n",
            "export const box = { item: { id: \"a\", note: \"memo\" } satisfies Row };\ntype Copy = typeof box;\nexport const copy: Copy = { item: { id: \"b\" } };\n",
        ),
        (
            "export const list = [{ id: \"a\" } satisfies Row];\ntype Copy = typeof list;\nexport const copy: Copy = [{ id: \"b\" }];\n",
            "export const list = [{ id: \"a\", note: \"memo\" } satisfies Row];\ntype Copy = typeof list;\nexport const copy: Copy = [{ id: \"b\" }];\n",
        ),
    ] {
        let import = "import type { Row } from \"./row\";\n\n";
        let (base, after) = (format!("{import}{base}"), format!("{import}{after}"));
        assert_eq!(
            row_verdict(
                &[("src/box.ts", &base)],
                ROW_AFTER,
                &[("src/box.ts", &after)]
            ),
            Verdict::Blocking,
            "{after}"
        );
    }

    // 入れ物から中身を取り出して別の名前へ束縛すると、その先の `typeof` は入れ物の名前の参照に
    // 現れない。キーの無い配列の分割代入、入れ物の中の入れ物の添字、名前の無い入れ物の添字。
    let contents = |note: &str, tail: &str| {
        [
            format!(
                "export const list = [{{ id: \"a\"{note} }} satisfies Row];\nconst [first] = list;\n{tail}"
            )
            .replace("SOURCE", "first"),
            format!(
                "export const outer = {{ inner: {{ item: {{ id: \"a\"{note} }} satisfies Row }} }};\nconst {{ inner }} = outer;\nconst it = inner[\"item\"];\n{tail}"
            )
            .replace("SOURCE", "it"),
            format!("export const sample = [{{ id: \"a\"{note} }} satisfies Row][0];\n{tail}")
                .replace("SOURCE", "sample"),
            // 識別子で書いていないキーの分割代入は、キー名の参照に現れない。
            format!(
                "export const box = {{ item: {{ id: \"a\"{note} }} satisfies Row }};\nconst {{ \"item\": picked }} = box;\n{tail}"
            )
            .replace("SOURCE", "picked"),
            format!(
                "export const box = {{ item: {{ id: \"a\"{note} }} satisfies Row }};\nconst key = \"item\";\nconst {{ [key]: picked }} = box;\n{tail}"
            )
            .replace("SOURCE", "picked"),
            format!(
                "export const list = [{{ id: \"a\"{note} }} satisfies Row];\nconst {{ 0: picked }} = list;\n{tail}"
            )
            .replace("SOURCE", "picked"),
        ]
    };
    let tail = "export const copy: typeof SOURCE = { id: \"old\" };\n";
    let import = "import type { Row } from \"./row\";\n\n";
    for (base, after) in contents("", tail)
        .into_iter()
        .zip(contents(", note: \"m\"", tail))
    {
        let (base, after) = (format!("{import}{base}"), format!("{import}{after}"));
        assert_eq!(
            row_verdict(&[("src/s.ts", &base)], ROW_AFTER, &[("src/s.ts", &after)]),
            Verdict::Blocking,
            "{after}"
        );
    }
    // 対照: 入れ物の中身を同じキーの名前で取り出し、そのメンバーを読むだけなら閉じる。値その
    // もののメンバーの読み出しも同じ。
    for body in [
        "export const box = { item: { id: \"a\"NOTE } satisfies Row };\nconst { item } = box;\nexport const shown = item.id;\n",
        "export const shown = ({ id: \"a\"NOTE } satisfies Row).id;\n",
    ] {
        let (base, after) = (
            format!("{import}{}", body.replace("NOTE", "")),
            format!("{import}{}", body.replace("NOTE", ", note: \"m\"")),
        );
        assert_eq!(
            row_verdict(&[("src/s.ts", &base)], ROW_AFTER, &[("src/s.ts", &after)]),
            Verdict::Closed,
            "{after}"
        );
    }

    // 解析できない埋め込みソース (`.vue`) に値を作る関数の名前があれば、`typeof` かどうかを
    // 確かめられない。
    let component = "<script setup lang=\"ts\">\nimport { toRow } from \"./row\";\ntype Made = ReturnType<typeof toRow>;\nconst made: Made = { id: \"v\" };\n</script>\n";
    assert_eq!(
        row_verdict(&[("src/Card.vue", component)], ROW_AFTER, &[]),
        Verdict::Blocking
    );

    // default export は任意の名前で import され、名前で `typeof` を探せない。
    let make_base = "import type { Row } from \"./row\";\n\nexport default function (): Row {\n    return { id: \"d\" };\n}\n";
    let make_after = "import type { Row } from \"./row\";\n\nexport default function (): Row {\n    return { id: \"d\", note: null };\n}\n";
    assert_eq!(
        row_verdict(
            &[("src/make.ts", make_base)],
            ROW_AFTER,
            &[("src/make.ts", make_after)]
        ),
        Verdict::Blocking
    );
}

/// 1 ファイルだけの base と変更後で Row の分類を返す。参照の行をすべて変更行にした
/// 対照で、「参照行が変更行にある」を証拠にしないことを確かめる (従来はこの形で閉じていた)。
fn single_file_verdict(base: &str, after: &str) -> Verdict {
    verdict(
        &api_after(&[("src/row.ts", base)], &[("src/row.ts", after)]),
        "Row",
    )
}

#[test]
fn unsupported_required_member_additions_do_not_close_on_changed_lines() {
    // toRow を 1 行で書き、Row の参照がある行をすべて変更行にする。
    let one_line = |members: &str, value: &str| {
        format!(
            "export interface Row {{\n    id: string;\n{members}}}\n\nexport function toRow(id: string): Row {{ return {value}; }}\n"
        )
    };
    let generic_base = "export interface Row<T> {\n    id: T;\n}\n\nexport function toRow(id: string): Row<string> { return { id }; }\n";
    let generic_after = "export interface Row<T> {\n    id: T;\n    note: string | null;\n}\n\nexport function toRow(id: string): Row<string> { return { id, note: null }; }\n";
    for (name, base, after) in [
        (
            "generic",
            generic_base.to_string(),
            generic_after.to_string(),
        ),
        (
            "required method",
            one_line("", "{ id }"),
            one_line("    show(): string;\n", "{ id, show: () => id }"),
        ),
        (
            "self reference",
            one_line("", "{ id }"),
            one_line("    parent: Row | null;\n", "{ id, parent: null }"),
        ),
        (
            // 宣言自体が型エラーになる (従来はこれも閉じていた)。
            "index signature",
            one_line("    [key: string]: string;\n", "{ id }"),
            one_line(
                "    [key: string]: string;\n    note: number;\n",
                "{ id, note: 1 }",
            ),
        ),
        (
            // 同時に足した optional メンバーと名前が衝突する (宣言自体が型エラー)。
            "name collision",
            one_line("", "{ id }"),
            one_line(
                "    note: string;\n    note?: number;\n",
                "{ id, note: \"memo\" }",
            ),
        ),
        (
            // 数値の名前は表記で比べられない (`1` と `0x1` は同じプロパティ)。
            "numeric name",
            one_line("    1?: number;\n", "{ id }"),
            one_line(
                "    1?: number;\n    0x1?: string;\n    note: string;\n",
                "{ id, note: \"memo\" }",
            ),
        ),
        (
            // 対応している形でも、証明できない位置 (`Row[]`) の参照は変更行にあっても閉じない。
            "array position",
            one_line("", "{ id }") + "export const rows: Row[] = [toRow(\"x\")];\n",
            one_line("    note: string | null;\n", "{ id, note: null }")
                + "export const rows: Row[] = [toRow(\"x\"), toRow(\"y\")];\n",
        ),
    ] {
        assert_eq!(
            single_file_verdict(&base, &after),
            Verdict::Blocking,
            "{name}\n{after}"
        );
    }
    // 対照: 同じ形で、証明できる位置の参照だけなら閉じる。
    assert_eq!(
        single_file_verdict(
            &one_line("", "{ id }"),
            &one_line("    note: string | null;\n", "{ id, note: null }"),
        ),
        Verdict::Closed
    );

    // optional プロパティだけの追加は従来どおり互換 (対照)。
    let optional = ROW_AFTER
        .replace("note: string | null;", "note?: string | null;")
        .replace("{ id, note: null }", "{ id }");
    let api = api_after(
        &[("src/row.ts", ROW_BASE), ("src/view.ts", VIEW_BASE)],
        &[("src/row.ts", &optional)],
    );
    assert!(
        api.compatible_modified.iter().any(|m| m.name == "Row"),
        "{api:?}"
    );
}

#[test]
fn required_member_value_types_follow_contextual_typing() {
    // 追加メンバーの型と、toRow が返す値。view.ts は note を読まない。
    let view_after = VIEW_AFTER.replace(
        "row.note === null ? row.id : `${row.id} (${row.note})`",
        "row.id",
    );
    let check = |member: &str, value: &str, row_base: &str, to_row: &str| {
        let after = format!(
            "export interface Row {{\n    id: string;\n    note: {member};\n}}\n\nexport function toRow({to_row}): Row {{\n    return {{ id: \"a\", note: {value} }};\n}}\n"
        );
        verdict(
            &api_after(
                &[("src/row.ts", row_base), ("src/view.ts", VIEW_BASE)],
                &[("src/row.ts", &after), ("src/view.ts", &view_after)],
            ),
            "Row",
        )
    };
    let base = |to_row: &str| {
        format!(
            "export interface Row {{\n    id: string;\n}}\n\nexport function toRow({to_row}): Row {{\n    return {{ id: \"a\" }};\n}}\n"
        )
    };
    let plain = base("id: string");
    for (member, value, expected) in [
        // bigint の literal 型は値ごとに別の型。
        ("1n", "1n", Verdict::Closed),
        ("1n", "2n", Verdict::Blocking),
        // `satisfies` は中の literal の文脈の型を変える (`string[]` になる)。
        ("\"ready\"[]", "[\"ready\"]", Verdict::Closed),
        (
            "\"ready\"[]",
            "[\"ready\"] satisfies string[]",
            Verdict::Blocking,
        ),
        // 配列への `as const` は readonly な tuple になり、配列型へ代入できない。
        ("string[]", "[\"a\"] as const", Verdict::Blocking),
        ("\"ready\"", "\"ready\" as const", Verdict::Closed),
        ("undefined", "undefined", Verdict::Closed),
    ] {
        assert_eq!(
            check(member, value, &plain, "id: string"),
            expected,
            "note: {member} = {value}"
        );
    }
    // 引数名で上書きした `undefined` は組み込みの値ではない。
    let shadowed = base("undefined: string");
    assert_eq!(
        check("undefined", "undefined", &shadowed, "undefined: string"),
        Verdict::Blocking
    );
}

#[test]
fn required_member_producers_in_classes_and_js_comments_stay_blocking() {
    let producer = |kind: &str, returned: &str| match kind {
        // クラスのメソッドの型は、クラスの型 (`Repo["find"]`・`implements`) を通じて写る。
        "class" => format!(
            "import type {{ Row }} from \"./row\";\n\nexport class Repo {{\n    find(): Row {{\n        return {returned};\n    }}\n}}\n"
        ),
        _ => format!(
            "import type {{ Row }} from \"./row\";\n\nexport const api = {{\n    make(): Row {{\n        return {returned};\n    }},\n}};\n"
        ),
    };
    for (kind, expected) in [("class", Verdict::Blocking), ("object", Verdict::Closed)] {
        let (base, after) = (
            producer(kind, "{ id: \"p\" }"),
            producer(kind, "{ id: \"p\", note: null }"),
        );
        assert_eq!(
            row_verdict(
                &[("src/make.ts", &base)],
                ROW_AFTER,
                &[("src/make.ts", &after)]
            ),
            expected,
            "{kind}"
        );
    }

    // checkJs では JSDoc も型として検査されるが、コメントは名前の参照に現れない。型は `{...}` で
    // 囲まなくても読まれる。
    for tag in [
        "@type {ReturnType<typeof toRow>}",
        "@type ReturnType<typeof toRow>",
    ] {
        let jsdoc = format!(
            "import {{ toRow }} from \"./row\";\n\n/** {tag} */\nexport const copy = {{ id: \"b\" }};\n"
        );
        assert_eq!(
            row_verdict(&[("src/copy.js", &jsdoc)], ROW_AFTER, &[]),
            Verdict::Blocking,
            "{tag}"
        );
    }
    // 対照: コメントに名前の無い JS の呼び出し側は、TS と同じく証明する。型として読まれない
    // 普通のコメントに名前があっても妨げない。
    let plain_js = "import { label } from \"./view\";\nimport { toRow } from \"./row\";\n\n// Row の一覧を作る\nexport const third = label(toRow(\"c\"));\n";
    assert_eq!(
        row_verdict(&[("src/use.js", plain_js)], ROW_AFTER, &[]),
        Verdict::Closed
    );
}

#[test]
fn required_member_string_access_keeps_blocking() {
    // 名前空間オブジェクトから文字列で値や型を取り出すと、名前の参照に現れない。
    for copy in [
        "import * as rows from \"./row\";\n\nconst make = rows[\"toRow\"];\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"old\" };\n",
        "import * as rows from \"./row\";\n\ntype Copy = ReturnType<(typeof rows)[\"toRow\"]>;\nexport const copy: Copy = { id: \"old\" };\n",
        "import * as rows from \"./row\";\n\nconst { \"toRow\": make } = rows;\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"old\" };\n",
        // 値を変えずに包む式・computed key・型の union と template の型を上っても、取り出す
        // 位置にある。
        "import * as rows from \"./row\";\n\nconst make = rows[\"toRow\" as const];\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"old\" };\n",
        "import * as rows from \"./row\";\n\nconst make = rows[(\"toRow\")];\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"old\" };\n",
        "import * as rows from \"./row\";\n\nconst { [\"toRow\"]: make } = rows;\ntype Copy = ReturnType<typeof make>;\nexport const copy: Copy = { id: \"old\" };\n",
        "import * as rows from \"./row\";\n\ntype Copy = ReturnType<(typeof rows)[\"toRow\" | \"toRow\"]>;\nexport const copy: Copy = { id: \"old\" };\n",
        "import * as rows from \"./row\";\n\ntype Copy = ReturnType<(typeof rows)[`toRow`]>;\nexport const copy: Copy = { id: \"old\" };\n",
        // 受け取る関数を文字列で取り出すと、その呼び出しを確かめられない。
        "import * as views from \"./view\";\n\nconst show = views[\"label\"];\nexport const out = show({ id: \"x\" });\n",
    ] {
        assert_eq!(
            row_verdict(&[("src/copy.ts", copy)], ROW_AFTER, &[]),
            Verdict::Blocking,
            "{copy}"
        );
    }
    // 対照: 追跡していない名前の文字列の添字、添字以外の位置の同じ文字列、名前空間を通した
    // 呼び出しは妨げない。
    for copy in [
        "const labels: Record<string, string> = { a: \"b\" };\nexport const shown = labels[\"other\"];\nexport const title = \"toRow\";\nconsole.log(\"label\");\n",
        "import * as rows from \"./row\";\n\nexport const made = rows.toRow(\"c\");\n",
    ] {
        assert_eq!(
            row_verdict(&[("src/copy.ts", copy)], ROW_AFTER, &[]),
            Verdict::Closed,
            "{copy}"
        );
    }
}

#[test]
fn required_member_ignores_value_names_and_intrinsic_jsx_elements() {
    // 外部パッケージの同名コンポーネント (値の名前空間) と、関数と同名の組み込み要素
    // (`<label>`) は、型の参照でも関数の呼び出しでもない。
    let extra = [
        (
            "src/grid.tsx",
            "import { Row } from \"react-bootstrap\";\n\nexport const grid = <Row />;\n",
        ),
        (
            "src/form.tsx",
            "export const form = <label htmlFor=\"x\">x</label>;\n",
        ),
    ];
    assert_eq!(row_verdict(&extra, ROW_AFTER, &[]), Verdict::Closed);
}

fn array_producer_verdict(signature: &str, body: &str, extra: &str) -> Verdict {
    let base = format!(
        "export interface Row {{\n    id: string;\n}}\nexport {signature} {{\n    return ids.map((id) => ({{ id }}));\n}}\n{extra}"
    );
    let after = format!(
        "export interface Row {{\n    id: string;\n    note: string | null;\n}}\nexport {signature} {{\n    {body}\n}}\n{extra}"
    );
    verdict(
        &api_after(&[("src/row.ts", &base)], &[("src/row.ts", &after)]),
        "Row",
    )
}

#[test]
fn required_member_array_producers_close_only_proven_map_returns() {
    for signature in [
        "function toRows(ids: string[]): Row[]",
        "function toRows(ids: readonly string[]): readonly Row[]",
        "function toRows(ids: Array<string>): Array<Row>",
        "async function toRows(ids: string[]): Promise<Row[]>",
        "async function toRows(ids: string[]): Promise<Array<Row>>",
    ] {
        for body in [
            "return ids.map((id) => ({ id, note: null }));",
            "return ids.map((id) => ({ id, note: id }));",
            "return ids.map(id => ({ id, note: null }));",
            "return ids.map((id) => { return { id, note: null }; });",
            "return ids.map((id) => { if (id) return { id, note: null }; return { id, note: null }; });",
            "return ids.map((id) => { if (id) return { id, note: null }; else return { id, note: null }; });",
        ] {
            assert_eq!(
                array_producer_verdict(signature, body, ""),
                Verdict::Closed,
                "{signature}: {body}"
            );
        }
    }
}

#[test]
fn required_member_array_producers_keep_unproven_values_blocking() {
    for body in [
        "return ids.map((id) => ({ id }));",
        "return ids.map((id) => ({ id, note: 123 }));",
        "return ids.map((id) => { id = undefined; return { id, note: id }; });",
        "return ids.map((id) => { if (id) return { id, note: null }; });",
        "if (ids.length) return ids.map((id) => ({ id, note: null }));",
        "if (ids.length) return [] as Row[]; return ids.map((id) => ({ id, note: null }));",
        "return ids.flatMap((id) => ({ id, note: null }));",
        "return ids.map(async (id) => ({ id, note: null }));",
        "return ids.map((id) => ({ id, note: null } as Row));",
        "return ids.map((id) => ({ id, note: null })) as Row[];",
        "return ids.map((id) => { const inner = () => ({ id, note: null }); return { id }; });",
        "ids.map = other; return ids.map((id) => ({ id, note: null }));",
    ] {
        assert_eq!(
            array_producer_verdict("function toRows(ids: string[]): Row[]", body, ""),
            Verdict::Blocking,
            "{body}"
        );
    }
    for signature in [
        "function toRows(ids: any): Row[]",
        "function toRows(ids: unknown): Row[]",
    ] {
        assert_eq!(
            array_producer_verdict(
                signature,
                "return ids.map((id) => ({ id, note: null }));",
                ""
            ),
            Verdict::Blocking,
            "{signature}"
        );
    }
    for (signature, extra) in [
        (
            "function toRows(ids: string[]): Array<Row>",
            "type Array<T> = T[];\n",
        ),
        (
            "async function toRows(ids: string[]): Promise<Row[]>",
            "type Promise<T> = T;\n",
        ),
        (
            "function toRows(ids: string[]): Box<Row>",
            "type Box<T> = T;\n",
        ),
        (
            "function toRows(ids: string[]): Row[]",
            "Array.prototype.map = customMap;\n",
        ),
    ] {
        assert_eq!(
            array_producer_verdict(
                signature,
                "return ids.map((id) => ({ id, note: null }));",
                extra
            ),
            Verdict::Blocking,
            "{signature}: {extra}"
        );
    }
}

#[test]
fn required_member_async_single_producer_closes_proven_return() {
    let base = ROW_BASE
        .replace("export function toRow", "export async function toRow")
        .replace("): Row {", "): Promise<Row> {");
    let after = ROW_AFTER
        .replace("export function toRow", "export async function toRow")
        .replace("): Row {", "): Promise<Row> {");
    let api = api_after(&[("src/row.ts", &base)], &[("src/row.ts", &after)]);
    assert_eq!(verdict(&api, "Row"), Verdict::Closed, "{api:?}");
}

#[test]
fn required_member_map_contextual_type_is_bounded_in_existing_producers() {
    let make = |value: &str, added: &str, outer: &str| {
        format!(
            "export interface Row {{\n    id: string;\n{added}}}\nexport function toRow(ids: string[]): Row {{\n    ids.map((id) => {{ const row: Row = {{ id{value} }}; return row; }});\n    return {{ id: 'x'{outer} }};\n}}\n"
        )
    };
    let base = make("", "", "");
    for (value, expected) in [("id", Verdict::Closed), ("123", Verdict::Blocking)] {
        let after = make(
            &format!(", note: {value}"),
            "    note: string | null;\n",
            ", note: null",
        );
        let api = api_after(&[("src/row.ts", &base)], &[("src/row.ts", &after)]);
        assert_eq!(verdict(&api, "Row"), expected, "{api:?}");
    }
    // 未対応の generic 戻り値を、裸の Row の既存経路で証明しない。
    let base = ROW_BASE.replace("): Row {", "): Box<Row> {") + "type Box<T> = T;\n";
    let after = ROW_AFTER.replace("): Row {", "): Box<Row> {") + "type Box<T> = T;\n";
    let api = api_after(&[("src/row.ts", &base)], &[("src/row.ts", &after)]);
    assert_eq!(verdict(&api, "Row"), Verdict::Blocking, "{api:?}");
}

#[test]
fn required_member_array_literal_callback_types_are_widened() {
    let base = "export interface Row {\n    id: string;\n}\nexport function toRows(): Row[] {\n    return ['ready'].map((id) => ({ id }));\n}\n";
    for (value, expected) in [("id", Verdict::Blocking), ("'ready'", Verdict::Closed)] {
        let after = format!(
            "export interface Row {{\n    id: string;\n    note: 'ready';\n}}\nexport function toRows(): Row[] {{\n    return ['ready'].map((id) => ({{ id, note: {value} }}));\n}}\n"
        );
        let api = api_after(&[("src/row.ts", base)], &[("src/row.ts", &after)]);
        assert_eq!(verdict(&api, "Row"), expected, "{api:?}");
    }
}

#[test]
fn required_member_builtin_type_uses_require_parseable_sources() {
    for (signature, good, bad) in [
        (
            "function toRows(ids: string[]): Row[]",
            "export const other: Array<string> = [];\n",
            "export const other: Array<string> = ;\n",
        ),
        (
            "async function toRows(ids: string[]): Promise<Row[]>",
            "export const other: Promise<string> = Promise.resolve('ok');\n",
            "export const other: Promise<string> = ;\n",
        ),
    ] {
        let base = format!(
            "export interface Row {{\n    id: string;\n}}\nexport {signature} {{\n    return ids.map((id) => ({{ id }}));\n}}\n"
        );
        let after = base
            .replace(
                "    id: string;",
                "    id: string;\n    note: string | null;",
            )
            .replace("({ id })", "({ id, note: null })");
        for (other, expected) in [(good, Verdict::Closed), (bad, Verdict::Blocking)] {
            let api = api_after(
                &[("src/row.ts", &base), ("src/other.ts", other)],
                &[("src/row.ts", &after)],
            );
            assert_eq!(verdict(&api, "Row"), expected, "{other}: {api:?}");
        }
    }
}

fn narrowing_verdict(row: &str, view: &str, extra: &[(&str, &str)]) -> Verdict {
    let mut base = vec![("src/row.ts", ROW_AFTER), ("src/view.ts", VIEW_AFTER)];
    base.extend_from_slice(extra);
    verdict(
        &api_after(&base, &[("src/row.ts", row), ("src/view.ts", view)]),
        "Row",
    )
}

#[test]
fn member_type_narrowing_closes_updated_producer_and_consumer() {
    let row = ROW_AFTER
        .replace("note: string | null", "note: string")
        .replace("note: null", "note: id");
    let view = VIEW_AFTER.replace("row.note === null ? row.id : ", "");
    assert_eq!(narrowing_verdict(&row, &view, &[]), Verdict::Closed);
}

#[test]
fn member_type_narrowing_keeps_stale_and_unproven_values_blocking() {
    let row = ROW_AFTER
        .replace("note: string | null", "note: string")
        .replace("note: null", "note: id");
    let view = VIEW_AFTER.replace("row.note === null ? row.id : ", "");
    for stale in [
        row.replace("note: id", "note: null"),
        row.replace("id: string): Row", "id: any): Row"),
        row.replace("note: id", "note: id as string"),
        row.replace("note: id", "note: id!"),
        row.replace(
            "return { id, note: id };",
            "if (id) return { id, note: id };",
        ),
        row.replace(
            "return { id, note: id };",
            "return { id, note: id } as Row;",
        ),
    ] {
        assert_eq!(
            narrowing_verdict(&stale, &view, &[]),
            Verdict::Blocking,
            "{stale}"
        );
    }
    assert_eq!(narrowing_verdict(&row, VIEW_AFTER, &[]), Verdict::Blocking);
    assert_eq!(
        narrowing_verdict(&row, &view.replace("): string {", ") {"), &[]),
        Verdict::Blocking
    );
    for statement in [
        "row.note = null;",
        "row.note = 'valid';",
        "row.note += '!';",
        "row.note++;",
        "delete row.note;",
        "({ note: row.note } = incoming);",
        "[row.note] = incoming;",
        "for (row.note of incoming) {}",
        "row['note'] = null;",
        "const alias = row; alias.note = null;",
        "const note = row.note;",
        "const { note } = row;",
        "unknown(row);",
        "unknown(row.note);",
        "Object.assign(row, { note: null });",
        "row.note === null;",
        "null !== row.note;",
        "row.note === undefined;",
        "const missing = null; row.note === missing;",
        "row.note!;",
    ] {
        let changed = view.replace("    return `", &format!("    {statement}\n    return `"));
        assert_eq!(
            narrowing_verdict(&row, &changed, &[]),
            Verdict::Blocking,
            "{statement}"
        );
    }
    for extra in [
        "import { toRow } from './row';\ntoRow('x').note = null;\n",
        "import { toRow } from './row';\ntoRow('x').note === null;\n",
        "import { toRow } from './row';\nconst alias = toRow('x');\nalias.note = null;\n",
        "import { toRow } from './row';\nunknown(toRow('x'));\n",
        "import { toRow } from './row';\ntoRow('x')['note'] = null;\n",
        "import { toRow } from './row';\nconst make = toRow;\n",
    ] {
        assert_eq!(
            narrowing_verdict(&row, &view, &[("src/extra.ts", extra)]),
            Verdict::Blocking,
            "{extra}"
        );
    }
}

#[test]
fn member_type_narrowing_accepts_only_strict_scalar_union_removals() {
    for (before, after, old_value, new_value) in [
        ("string | undefined", "string", "undefined", "'ready'"),
        ("'ready' | 'done'", "'ready'", "'done'", "'ready'"),
        ("true | false", "true", "false", "true"),
        ("1n | 2n", "1n", "2n", "1n"),
    ] {
        let make = |ty: &str, value: &str| {
            format!(
                "export interface Row {{\n    note: {ty};\n}}\nexport function toRow(): Row {{\n    return {{ note: {value} }};\n}}\n"
            )
        };
        let (base, updated) = (make(before, old_value), make(after, new_value));
        let api = api_after(&[("src/row.ts", &base)], &[("src/row.ts", &updated)]);
        assert_eq!(
            verdict(&api, "Row"),
            Verdict::Closed,
            "{before} -> {after}: {api:?}"
        );
    }
    let row = ROW_AFTER
        .replace("note: string | null", "note: string")
        .replace("note: null", "note: id");
    let view = VIEW_AFTER.replace("row.note === null ? row.id : ", "");
    for unsupported in [
        row.replace("note: string", "note: number")
            .replace("note: id", "note: 1"),
        row.replace("note: string", "note: string | number | null"),
        row.replace("note: string", "readonly note: string"),
        row.replace(
            "export interface Row {",
            "export interface Row<T = string> {",
        ),
        row.replace(
            "    note: string;",
            "    note: string;\n    extra?: string;",
        ),
    ] {
        // 参照行をすべて変更しても、未知の型変更は行の証拠へ戻さない。
        let changed = unsupported.replace("): Row {", "): Row { // updated");
        let view_changed = view.replace("row: Row", "row : Row");
        assert_eq!(
            narrowing_verdict(&changed, &view_changed, &[]),
            Verdict::Blocking,
            "{changed}"
        );
    }
}

#[test]
fn member_type_narrowing_unknown_names_do_not_fall_back_to_changed_lines() {
    for key in ["\"note\"", "1"] {
        let base = ROW_AFTER
            .replace("note: string | null", &format!("{key}: string | null"))
            .replace("note: null", &format!("{key}: null"));
        let changed = base
            .replace("string | null", "string")
            .replace("): Row {", "): Row { // updated type reference");
        let view = if key == "1" {
            VIEW_AFTER.replace("row.note", "row[1]")
        } else {
            VIEW_AFTER.to_string()
        };
        let view_changed = view.replace("row: Row", "row : Row");
        let api = api_after(
            &[("src/row.ts", &base), ("src/view.ts", &view)],
            &[("src/row.ts", &changed), ("src/view.ts", &view_changed)],
        );
        assert_eq!(verdict(&api, "Row"), Verdict::Blocking, "{key}: {api:?}");
    }
}

#[test]
fn member_type_narrowing_method_producers_are_unproven() {
    let view = VIEW_AFTER.replace("row.note === null ? row.id : ", "");
    let base = ROW_AFTER
        .replace(
            "    note: string | null;",
            "    note: string | null;\n    reset: () => void;",
        )
        .replace(
            "return { id, note: null };",
            "return { id, note: null, reset() { this.note = null; } };",
        );
    let after = base
        .replace("note: string | null", "note: string")
        .replace("id, note: null,", "id, note: id,");
    let api = api_after(
        &[("src/row.ts", &base), ("src/view.ts", VIEW_AFTER)],
        &[("src/row.ts", &after), ("src/view.ts", &view)],
    );
    assert_eq!(verdict(&api, "Row"), Verdict::Blocking, "{api:?}");

    // スカラーだけの型でも、getter が this へ書き込む producer は証明しない。
    let base = ROW_AFTER.replace(
        "return { id, note: null };",
        "return { get id() { this.note = null; return id; }, note: null };",
    );
    let after = base
        .replace("note: string | null", "note: string")
        .replace("}, note: null", "}, note: id");
    let api = api_after(
        &[("src/row.ts", &base), ("src/view.ts", VIEW_AFTER)],
        &[("src/row.ts", &after), ("src/view.ts", &view)],
    );
    assert_eq!(verdict(&api, "Row"), Verdict::Blocking, "{api:?}");
}

#[test]
fn member_type_narrowing_local_annotations_do_not_hide_unsafe_initializers() {
    let row = ROW_AFTER
        .replace("note: string | null", "note: string")
        .replace("note: null", "note: note");
    let view = VIEW_AFTER.replace("row.note === null ? row.id : ", "");
    for prefix in [
        "const maybe: string | null = null; const note: string = maybe!;",
        "const sourceAny: any = null; const note: string = sourceAny;",
        "const note: string = id as string;",
        "const note: string = 123;",
        "const a: string = b; const b: string = a; const note: string = a;",
        "let note: string = id; note = sourceAny;",
        "const sourceAny: any = null; id = sourceAny; const note: string = id;",
    ] {
        let after = row.replace("    return {", &format!("    {prefix}\n    return {{"));
        assert_eq!(
            narrowing_verdict(&after, &view, &[]),
            Verdict::Blocking,
            "{prefix}"
        );
    }
    for prefix in [
        "const note: string = id;",
        "const first: string = id; const note: string = first;",
    ] {
        let after = row.replace("    return {", &format!("    {prefix}\n    return {{"));
        assert_eq!(
            narrowing_verdict(&after, &view, &[]),
            Verdict::Closed,
            "{prefix}"
        );
    }
    let mut chain = String::from("const n0: string = id; ");
    for index in 1..=17 {
        chain.push_str(&format!("const n{index}: string = n{}; ", index - 1));
    }
    chain.push_str("const note: string = n17;");
    let after = row.replace("    return {", &format!("    {chain}\n    return {{"));
    assert_eq!(narrowing_verdict(&after, &view, &[]), Verdict::Blocking);
    for (default, expected) in [("123", Verdict::Blocking), ("'fallback'", Verdict::Closed)] {
        let after = row
            .replace("id: string): Row", &format!("id: string = {default}): Row"))
            .replace("note: note", "note: id");
        assert_eq!(
            narrowing_verdict(&after, &view, &[]),
            expected,
            "default {default}"
        );
    }
}

#[test]
fn member_type_narrowing_ambiguous_declarations_remain_blocking() {
    let extra = "\nnamespace N { interface Row { other: string; } }\n";
    let base = ROW_AFTER.to_string() + extra;
    let after = base
        .replace("note: string | null", "note: string")
        .replace("): Row {", "): Row { // updated reference");
    let view = VIEW_AFTER.replace("row: Row", "row : Row");
    let api = api_after(
        &[("src/row.ts", &base), ("src/view.ts", VIEW_AFTER)],
        &[("src/row.ts", &after), ("src/view.ts", &view)],
    );
    assert_eq!(verdict(&api, "Row"), Verdict::Blocking, "{api:?}");
}
