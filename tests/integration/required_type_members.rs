//! TypeScript の interface へ必須プロパティを足し、値を作る箇所を同じ差分で直したときの
//! `review --git --hook` (Issue #89)。

use super::support::{TestRepo, cargo_bin};

#[test]
fn property_removal_preserves_unchanged_callable_members() {
    for extension in ["ts", "tsx"] {
        for member in ["render(): string;", "readonly render: () => string;"] {
            let repo = TestRepo::new();
            repo.init_git();
            repo.create_dir_all("src");
            let options = format!(
                "export interface Options {{\n readonly size: number;\n readonly label: string;\n {member}\n}}\n"
            );
            let defaults = "import type { Options } from './options';\nexport function defaults(): Options {\n return { size: 1, label: 'a', render: () => 'x' };\n}\n";
            let target = format!("src/options.{extension}");
            let factory = format!("src/defaults.{extension}");
            repo.write(&target, &options);
            repo.write(&factory, defaults);
            repo.commit_all("initial");
            repo.write(&target, options.replace(" readonly label: string;\n", ""));
            repo.write(&factory, defaults.replace(", label: 'a'", ""));
            for staged in [false, true] {
                if staged {
                    repo.stage_all();
                    repo.write(&factory, "unstaged invalid source");
                }
                let mut args = vec!["--git", "--hook"];
                if staged {
                    args.push("--staged");
                }
                let output = cargo_bin()
                    .args(["review", "--dir"])
                    .arg(repo.root())
                    .args(args)
                    .output()
                    .unwrap();
                let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert!(
                    output.status.success(),
                    "{extension} {member} {staged}: {json}"
                );
                assert_eq!(json["blocking_categories"], serde_json::json!([]), "{json}");
                assert!(
                    json["api"]["mod_closed"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["n"] == "Options"),
                    "{json}"
                );
            }
        }
    }
}

#[test]
fn property_removal_callable_members_keep_unproven_uses_blocking() {
    for member in ["render(): string;", "readonly render: () => string;"] {
        for (case, usage, callback) in [
            (
                "read",
                "export function label(value: Options): string { return value.label; }\n",
                "render: () => 'x'",
            ),
            (
                "type key",
                "export type Label = Options['label'];\n",
                "render: () => 'x'",
            ),
            ("changed implementation", "", "render: () => 'y'"),
            ("missing callback", "", ""),
        ] {
            let repo = TestRepo::new();
            repo.init_git();
            repo.create_dir_all("src");
            let options = format!(
                "export interface Options {{\n size: number;\n label: string;\n {member}\n}}\n{usage}"
            );
            let defaults = "import type { Options } from './options';\nexport function defaults(): Options {\n return { size: 1, label: 'a', render: () => 'x' };\n}\n";
            repo.write("src/options.ts", &options);
            repo.write("src/defaults.ts", defaults);
            repo.commit_all("initial");
            repo.write("src/options.ts", options.replace(" label: string;\n", ""));
            repo.write(
                "src/defaults.ts",
                defaults
                    .replace(", label: 'a'", "")
                    .replace("render: () => 'x'", callback),
            );
            for staged in [false, true] {
                if staged {
                    repo.stage_all();
                    repo.write("src/defaults.ts", "unstaged invalid source");
                }
                let mut args = vec!["--git", "--hook"];
                if staged {
                    args.push("--staged");
                }
                let output = cargo_bin()
                    .args(["review", "--dir"])
                    .arg(repo.root())
                    .args(args)
                    .output()
                    .unwrap();
                let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(1),
                    "{member} {case} {staged}: {json}"
                );
                assert!(
                    json["api"]["mod"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["n"] == "Options"),
                    "{json}"
                );
            }
        }
    }
}

#[test]
fn typed_array_literals_close_required_member_additions() {
    for extension in ["ts", "tsx"] {
        for (annotation, literal) in [
            ("Causes[]", "[$VALUE]"),
            ("Array<[Causes, string]>", "[[$VALUE, 'first']]"),
        ] {
            typed_array_case(extension, annotation, literal, "", "", 0);
        }
    }
}

#[test]
fn typed_array_container_forms_prove_all_target_positions() {
    for extension in ["ts", "tsx"] {
        for (annotation, literal) in [
            ("readonly Causes[]", "[$VALUE]"),
            ("Array<Causes>", "[$VALUE]"),
            ("ReadonlyArray<Causes>", "[$VALUE]"),
            ("[Causes, string][]", "[[$VALUE, 'first']]"),
            ("Array<[string, Causes]>", "[['first', $VALUE]]"),
            ("Array<[Causes, Causes]>", "[[$VALUE, $VALUE]]"),
            ("Causes[][]", "[[$VALUE]]"),
            ("Causes[]", "[]"),
        ] {
            typed_array_case(extension, annotation, literal, "", "", 0);
        }
    }
    typed_array_case(
        "ts",
        "Causes[]",
        "[$VALUE]",
        "const length = rows.length; consume(length);",
        "",
        0,
    );
    typed_array_case(
        "ts",
        "Causes[]",
        "[build()]",
        "",
        "function build(): Causes {\n return $VALUE;\n}\n",
        0,
    );
}

#[test]
fn typed_array_variables_keep_missing_values_and_escapes_blocking() {
    for (annotation, literal, suffix, header) in [
        ("Causes[]", "[$BAD]", "", ""),
        ("Causes[]", "[$VALUE, $BAD]", "", ""),
        ("Array<[Causes, string]>", "[[$BAD, 'first']]", "", ""),
        ("Array<[Causes, Causes]>", "[[$VALUE, $BAD]]", "", ""),
        ("Array<[Causes, Causes]>", "[[$BAD, $VALUE]]", "", ""),
        ("Causes[]", "[$VALUE]", "rows[0] = $BAD;", ""),
        ("Causes[]", "[$VALUE]", "delete (rows[0] as any).third;", ""),
        ("Causes[]", "[$VALUE]", "rows.length = 0;", ""),
        ("Causes[]", "[$VALUE]", "rows.push($BAD);", ""),
        ("Causes[]", "[$VALUE]", "consume(rows);", ""),
        ("Causes[]", "[$VALUE]", "const alias = rows;", ""),
        ("Causes[]", "[$VALUE]", "const alias = rows[0];", ""),
        ("Causes[]", "[$VALUE]", "type Original = typeof rows;", ""),
        (
            "Causes[]",
            "[$VALUE]",
            "type Original = typeof rows[0];",
            "",
        ),
        ("Causes[]", "[...[], $VALUE]", "", ""),
        ("Causes[]", "[, $VALUE]", "", ""),
        ("Array<[Causes, string]>", "[[$VALUE]]", "", ""),
        ("Array<[Causes, string?]>", "[[$VALUE, 'first']]", "", ""),
        ("Array<Causes>", "[$VALUE]", "", "type Array<T> = T[];"),
        (
            "Causes[]",
            "[$VALUE]",
            "",
            "function unused(value: Causes): number { return value.first ? 1 : 0; }",
        ),
    ] {
        typed_array_case("ts", annotation, literal, suffix, header, 1);
    }
}

fn typed_array_case(
    extension: &str,
    annotation: &str,
    literal: &str,
    suffix: &str,
    header: &str,
    expected: i32,
) {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    let target = format!("src/causes.{extension}");
    let table = format!("src/table.{extension}");
    let old = "{ first: true, second: false }";
    let new = "{ first: true, third: false, second: false }";
    let source = |changed: bool| {
        let values = literal
            .replace("$VALUE", if changed { new } else { old })
            .replace("$BAD", old);
        let suffix = suffix
            .replace("$VALUE", if changed { new } else { old })
            .replace("$BAD", old);
        let header = header
            .replace("$VALUE", if changed { new } else { old })
            .replace("$BAD", old);
        format!(
            "import type {{ Causes }} from './causes';\n{header}\ndeclare function consume(value: unknown): void;\nexport function countRows(): number {{\n const rows: {annotation} = {values};\n {suffix}\n return rows.length;\n}}\n"
        )
    };
    repo.write(
        &target,
        "export interface Causes {\n first: boolean;\n second: boolean;\n}\n",
    );
    repo.write(&table, source(false));
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        &target,
        "export interface Causes {\n first: boolean;\n third: boolean;\n second: boolean;\n}\n",
    );
    repo.write(&table, source(true));
    let normal = repo.run_json("review", &["--git"]);
    let bucket = if expected == 0 {
        "modified_closed_in_diff"
    } else {
        "modified"
    };
    assert!(
        normal["api_changes"][bucket]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|c| c["name"] == "Causes")),
        "{annotation}/{literal}/{suffix}/{header}: {normal}"
    );
    let (code, json) = hook(&repo);
    assert_eq!(
        code, expected,
        "{annotation}/{literal}/{suffix}/{header}: {json}"
    );
    repo.stage_all();
    repo.write(&table, "import type { Causes } from './causes';\nexport function countRows(): number { const rows: Causes[] = [{ first: true, second: false }]; return rows.length; }\n");
    let output = cargo_bin()
        .args(["review", "--git", "--staged", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{annotation}/{literal}/{suffix}/{header}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn partial_factories_close_required_additions_with_typed_defaults() {
    for extension in ["ts", "tsx"] {
        for (before_params, after_params, labels) in [
            (
                "overrides: Partial<Info> = {}",
                "overrides: Partial<Info> = {}",
                "overrides.labels ?? []",
            ),
            (
                "overrides: Partial<Info> = {}",
                "overrides: Partial<Info> = {}",
                "[]",
            ),
            (
                "overrides: { tags?: string[]; labels?: string[]; ready?: boolean } = {}",
                "overrides: { tags?: string[]; labels?: string[]; ready?: boolean } = {}",
                "overrides.labels ?? []",
            ),
            ("", "", "[]"),
        ] {
            let repo = TestRepo::new();
            repo.create_dir_all("src");
            let info = format!("src/info.{extension}");
            let factory = format!("src/factory.{extension}");
            repo.write(
                &info,
                "export interface Info {\n tags: string[];\n ready: boolean;\n}\n",
            );
            let value = if before_params.is_empty() {
                "tags: [], ready: false"
            } else {
                "tags: overrides.tags ?? [], ready: overrides.ready ?? false"
            };
            repo.write(&factory, format!("import type {{ Info }} from './info';\nexport function makeInfo({before_params}): Info {{\n return {{ {value} }};\n}}\n"));
            repo.write(
                "src/app.ts",
                "import { makeInfo } from './factory';\nexport const value = makeInfo();\n",
            );
            repo.init_git();
            repo.commit_all("initial");
            repo.write(&info, "export interface Info {\n tags: string[];\n labels: string[];\n ready: boolean;\n}\n");
            repo.write(&factory, format!("import type {{ Info }} from './info';\nexport function makeInfo({after_params}): Info {{\n return {{ {value}, labels: {labels} }};\n}}\n"));
            for staged in [false, true] {
                if staged {
                    repo.stage_all();
                }
                let mut args = vec!["--git"];
                if staged {
                    args.push("--staged");
                }
                let normal = repo.run_json("review", &args);
                assert!(
                    normal["api_changes"]["modified_closed_in_diff"]
                        .as_array()
                        .is_some_and(|changes| changes.iter().any(|c| c["name"] == "Info")),
                    "{after_params}/{labels}: {normal}"
                );
                let output = cargo_bin()
                    .arg("review")
                    .args(&args)
                    .args(["--hook", "--dir"])
                    .arg(repo.root())
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(0),
                    "{after_params}/{labels}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

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

#[test]
fn partial_factories_preserve_unknown_types_and_unsafe_defaults() {
    for (parameter, labels, definitions, prefix) in [
        ("overrides: Partial<Info> = {}", None, "", ""),
        (
            "overrides: Partial<Info> = {}",
            Some("overrides.labels"),
            "",
            "",
        ),
        (
            "overrides: Partial<Info> = {}",
            Some("overrides.labels ?? [1]"),
            "",
            "",
        ),
        (
            "overrides: Partial<Info> = {}",
            Some("overrides.labels || []"),
            "",
            "",
        ),
        (
            "overrides: Partial<Info> = {}",
            Some("overrides.labels ?? undefined"),
            "",
            "",
        ),
        (
            "overrides: Readonly<Info> = { tags: [], ready: false }",
            Some("[]"),
            "",
            "",
        ),
        (
            "overrides: Required<Partial<Info>> = { tags: [], ready: false }",
            Some("[]"),
            "",
            "",
        ),
        (
            "overrides: Partial<Info> = {}",
            Some("overrides.labels ?? []"),
            "type Partial<T> = { tags?: string[]; ready?: boolean; labels?: number[] };",
            "",
        ),
        (
            "overrides: Partial<Info> = {}",
            Some("[]"),
            "",
            "overrides.labels = [1] as any;",
        ),
        (
            "overrides: Partial<Info> = {}",
            Some("[]"),
            "",
            "const alias = overrides;",
        ),
        (
            "overrides: Partial<Info> = {}",
            Some("[]"),
            "declare function mutate(value: unknown): void;",
            "mutate(overrides);",
        ),
    ] {
        partial_factory_case(parameter, labels, definitions, prefix, "makeInfo()", 1);
    }
}

#[test]
fn partial_factories_check_present_optional_values_at_observed_calls() {
    partial_factory_case(
        "overrides: { tags?: string[]; labels?: string[]; ready?: boolean } = {}",
        Some("[]"),
        "declare function unrelated(): void;",
        "unrelated();",
        "makeInfo()",
        0,
    );
    partial_factory_case(
        "overrides: { tags?: string[]; labels?: string[]; ready?: boolean } = { labels: [] }",
        Some("[]"),
        "",
        "",
        "makeInfo()",
        0,
    );
    for (call, expected) in [
        ("", 0),
        ("makeInfo()", 0),
        ("makeInfo({})", 0),
        ("makeInfo({ labels: [] })", 0),
        ("makeInfo({ labels: ['a'] })", 0),
        ("makeInfo({ labels: [1] })", 1),
        ("makeInfo({ labels: undefined })", 1),
        ("makeInfo({ ...other })", 1),
        ("makeInfo(other)", 1),
    ] {
        partial_factory_case(
            "overrides: Partial<Info> = {}",
            Some("overrides.labels ?? []"),
            "",
            "",
            call,
            expected,
        );
    }
}

fn partial_factory_case(
    parameter: &str,
    labels: Option<&str>,
    definitions: &str,
    prefix: &str,
    call: &str,
    expected: i32,
) {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/info.ts",
        "export interface Info {\n tags: string[];\n ready: boolean;\n}\n",
    );
    repo.write("src/factory.ts", format!("import type {{ Info }} from './info';\n{definitions}\nexport function makeInfo({parameter}): Info {{\n return {{ tags: overrides.tags ?? [], ready: overrides.ready ?? false }};\n}}\n"));
    if !call.is_empty() {
        repo.write(
            "src/app.ts",
            "import { makeInfo } from './factory';\nconst other = { tags: [] };\nmakeInfo();\n",
        );
    }
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/info.ts",
        "export interface Info {\n tags: string[];\n labels: string[];\n ready: boolean;\n}\n",
    );
    let property = labels.map_or(String::new(), |value| format!(", labels: {value}"));
    repo.write("src/factory.ts", format!("import type {{ Info }} from './info';\n{definitions}\nexport function makeInfo({parameter}): Info {{\n {prefix}\n return {{ tags: overrides.tags ?? [], ready: overrides.ready ?? false{property} }};\n}}\n"));
    if !call.is_empty() {
        repo.write(
            "src/app.ts",
            format!(
                "import {{ makeInfo }} from './factory';\nconst other = {{ tags: [] }};\n{call};\n"
            ),
        );
    }
    let normal = repo.run_json("review", &["--git"]);
    let bucket = if expected == 0 {
        "modified_closed_in_diff"
    } else {
        "modified"
    };
    assert!(
        normal["api_changes"][bucket]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|c| c["name"] == "Info")),
        "{parameter}/{labels:?}/{prefix}/{call}: {normal}"
    );
    let (code, output) = hook(&repo);
    assert_eq!(
        code, expected,
        "{parameter}/{labels:?}/{prefix}/{call}: {output}"
    );
    repo.stage_all();
    let output = cargo_bin()
        .args(["review", "--git", "--staged", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{parameter}/{labels:?}/{prefix}/{call}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn partial_factory_fallthrough_stays_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("src/info.ts", "export interface Info { ready: boolean; }\n");
    repo.write("src/factory.ts", "import type { Info } from './info';\ndeclare const condition: boolean;\nexport function makeInfo(overrides: Partial<Info> = {}): Info {\n return { ready: false };\n}\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/info.ts",
        "export interface Info { ready: boolean; labels: string[]; }\n",
    );
    repo.write("src/factory.ts", "import type { Info } from './info';\ndeclare const condition: boolean;\nexport function makeInfo(overrides: Partial<Info> = {}): Info {\n if (condition) { return { ready: false, labels: overrides.labels ?? [] }; }\n}\n");
    let (code, json) = hook(&repo);
    assert_eq!(code, 1, "{json}");
    assert!(
        json["api"]["mod"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|c| c["n"] == "Info")),
        "{json}"
    );
}

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

#[test]
fn property_removal_preserves_unchanged_non_scalar_values_in_worktree_and_index() {
    for extension in ["ts", "tsx"] {
        for (annotation, value, changed_value) in [
            ("string[]", "[]", "['changed']"),
            ("readonly string[]", "[]", "['changed']"),
            ("Array<string>", "[]", "['changed']"),
            ("ReadonlyArray<string>", "[]", "['changed']"),
            ("Record<string, number>", "{}", "{ changed: 1 }"),
        ] {
            for case in [
                "updated",
                "value changed",
                "reader remains",
                "parameter changed",
            ] {
                if case != "updated" && !matches!(annotation, "string[]" | "Record<string, number>")
                {
                    continue;
                }
                let repo = TestRepo::new();
                repo.init_git();
                repo.create_dir_all("src");
                let row_path = format!("src/row.{extension}");
                let view_path = format!("src/view.{extension}");
                let row = format!(
                    "export interface Row {{\n    id: string;\n    data: {annotation};\n    extra: number;\n}}\nexport function toRow(id: string): Row {{\n    return {{ id, data: {value}, extra: 0 }};\n}}\n"
                );
                let view = "import { type Row, toRow } from './row';\nexport function label(row: Row): string {\n    return `${row.id} (${row.extra})`;\n}\nexport const first = label(toRow('a'));\n";
                repo.write(&row_path, &row);
                repo.write(&view_path, view);
                repo.commit_all("initial");
                let mut after = row
                    .replace("    extra: number;\n", "")
                    .replace(", extra: 0", "");
                if case == "value changed" {
                    after =
                        after.replace(&format!("data: {value}"), &format!("data: {changed_value}"));
                }
                if case == "parameter changed" {
                    after = after.replace("id: string): Row", "id: number): Row");
                }
                repo.write(&row_path, after);
                repo.write(
                    &view_path,
                    if case == "reader remains" {
                        view.to_string()
                    } else {
                        view.replace(" (${row.extra})", "")
                    },
                );
                let expected = i32::from(case != "updated");
                let category = if expected == 0 { "mod_closed" } else { "mod" };
                let (code, json) = hook(&repo);
                assert_eq!(code, expected, "{extension}/{annotation}/{case}: {json}");
                assert!(
                    json["api"][category]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["n"] == "Row"),
                    "{json}"
                );
                if expected == 0 {
                    let normal = repo.run_json("review", &["--git"]);
                    assert_eq!(
                        normal["api_changes"]["modified_closed_in_diff"][0]["name"], "Row",
                        "{normal}"
                    );
                }
                repo.stage_all();
                repo.write(&row_path, "export const changedOnlyInWorktree = 1;\n");
                let output = cargo_bin()
                    .args(["review", "--git", "--staged", "--hook", "--dir"])
                    .arg(repo.root())
                    .output()
                    .unwrap();
                let staged: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(expected),
                    "{extension}/{annotation}/{case}: {staged}"
                );
                assert!(
                    staged["api"][category]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["n"] == "Row"),
                    "{staged}"
                );
            }
        }
    }
}

#[test]
fn non_scalar_property_removal_rejects_optional_value_changes_and_unproven_producers() {
    for case in [
        "optional stable",
        "optional changed",
        "root const",
        "local const",
        "literal argument",
        "assertion",
        "named element type",
        "global builtin",
        "removed literal key",
        "new producer",
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        let mut row = "export interface Row {\n    id: string;\n    data?: string[];\n    extra: number;\n}\nexport function toRow(id: string): Row {\n    return { id, data: [], extra: 0 };\n}\n".to_string();
        let mut view = "import { type Row, toRow } from './row';\nexport function label(row: Row): string {\n    return `${row.id} (${row.extra})`;\n}\nexport const first = label(toRow('a'));\n".to_string();
        if case == "root const" {
            row += "const stored: Row = { id: 'fixed', data: [], extra: 0 };\nexport const second = stored.id;\n";
        }
        if case == "local const" {
            row = row.replace(
                "return { id, data: [], extra: 0 };",
                "const local: Row = { id, data: [], extra: 0 }; return local;",
            );
        }
        if case == "literal argument" {
            view += "export const second = label({ id: 'literal', data: [], extra: 0 });\n";
        }
        if case == "assertion" {
            row = row.replace("data: []", "data: [] as string[]");
        }
        if case == "named element type" {
            row = format!(
                "type Tag = string;\n{}",
                row.replace("data?: string[]", "data?: Tag[]")
            );
        }
        if case == "global builtin" {
            row = row
                .replace("data?: string[]", "data?: Record<string, number>")
                .replace("data: []", "data: {}");
        }
        repo.write("src/row.ts", &row);
        repo.write("src/view.ts", &view);
        if case == "global builtin" {
            repo.write("src/global.ts", "interface Record<K, V> { unrelated: V }\n");
        }
        repo.commit_all("initial");
        let mut after = row
            .replace("    extra: number;\n", "")
            .replace(", extra: 0", "");
        if case == "optional changed" {
            after = after.replace("data: []", "data: ['changed']");
        }
        if case == "removed literal key" {
            after = after.replace("data: []", "data: [], 'extra': 0");
        }
        if case == "new producer" {
            after += "export const additional: Row = { id: 'new', data: [] };\n";
        }
        repo.write("src/row.ts", after);
        repo.write(
            "src/view.ts",
            view.replace(" (${row.extra})", "")
                .replace(", extra: 0", ""),
        );
        let expected = i32::from(!matches!(case, "optional stable" | "root const"));
        let (code, json) = hook(&repo);
        assert_eq!(code, expected, "{case}: {json}");
        let bucket = if expected == 0 { "mod_closed" } else { "mod" };
        assert!(
            json["api"][bucket]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["n"] == "Row"),
            "{case}: {json}"
        );
    }
}

#[test]
fn property_removal_builtin_audit_allows_value_reads_and_rejects_type_shadowing() {
    for (annotation, value, utility, expected) in [
        (
            "string[]",
            "[]",
            "export const ok = (value: unknown) => Array.isArray(value);\ninterface Record<K, V> { unrelated: V }",
            0,
        ),
        (
            "Array<string>",
            "[]",
            "export const ok = (value: unknown) => Array.isArray(value);\nexport const values = Array.from('abc');\nexport const empty = new Array();",
            0,
        ),
        (
            "Record<string, number>",
            "{}",
            "export const ok = (value: unknown) => Array.isArray(value);",
            0,
        ),
        ("Array<string>", "[]", "type Array<T> = { custom: T };", 1),
        (
            "Record<string, number>",
            "{}",
            "import type { Record } from './foreign';",
            1,
        ),
        ("Array<string>", "[]", "export { Array };", 1),
        ("Array<string>", "[]", "const Alias = Array;", 1),
        ("Array<string>", "[]", "Array = replacement;", 1),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        let row = format!(
            "export interface Row {{ id: string; data: {annotation}; extra: number }}\nexport function toRow(id: string): Row {{ return {{ id, data: {value}, extra: 0 }}; }}\n"
        );
        let view = "import { type Row, toRow } from './row';\nexport function label(row: Row): string { return `${row.id} (${row.extra})`; }\nexport const first = label(toRow('a'));\n";
        repo.write("src/row.ts", &row);
        repo.write("src/view.ts", view);
        repo.write("src/util.ts", utility);
        if utility.starts_with("import") {
            repo.write(
                "src/foreign.ts",
                "export type Record<K, V> = { custom: V };\n",
            );
        }
        repo.commit_all("initial");
        repo.write(
            "src/row.ts",
            row.replace(" extra: number", "").replace(", extra: 0", ""),
        );
        repo.write("src/view.ts", view.replace(" (${row.extra})", ""));
        let (code, json) = hook(&repo);
        let bucket = if expected == 0 { "mod_closed" } else { "mod" };
        assert_eq!(code, expected, "{annotation}/{utility}: {json}");
        assert!(
            json["api"][bucket]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["n"] == "Row"),
            "{json}"
        );
    }
}

#[test]
fn property_removal_keeps_removed_non_scalar_members_outside_the_proof() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let row = "export interface Row { id: string; extra: string[] }\nexport function toRow(id: string): Row { return { id, extra: [] }; }\n";
    let view = "import { type Row, toRow } from './row';\nexport function label(row: Row): string { return `${row.id} (${row.extra.length})`; }\nexport const first = label(toRow('a'));\n";
    repo.write("src/row.ts", row);
    repo.write("src/view.ts", view);
    repo.commit_all("initial");
    repo.write(
        "src/row.ts",
        row.replace(" extra: string[]", "")
            .replace(", extra: []", ""),
    );
    repo.write("src/view.ts", view.replace(" (${row.extra.length})", ""));
    let (code, json) = hook(&repo);
    assert_eq!(code, 1, "{json}");
    assert_eq!(json["api"]["mod"][0]["n"], "Row", "{json}");
}

#[test]
fn type_modifications_explain_unverified_proofs_without_changing_classification() {
    for extension in ["ts", "tsx"] {
        for case in [
            "missing",
            "closed",
            "asserted value",
            "function",
            "union added",
            "union closed",
        ] {
            let repo = TestRepo::new();
            repo.create_dir_all("src");
            let target = format!("src/item.{extension}");
            let (before, after, producer_before, producer_after, application, name) = match case {
                "function" => (
                    "export function run(value: number): number {\nreturn value;\n}\n",
                    "export function run(value: number, label: string): number {\nreturn value;\n}\n",
                    "",
                    "",
                    "import { run } from './item';\nexport const result = run(1);\n",
                    "run",
                ),
                "union added" => (
                    "export type Level = 'low' | 'high';\nexport interface Alert { level: Level; }\n",
                    "export type Level = 'low' | 'middle' | 'high';\nexport interface Alert { level: Level; }\n",
                    "",
                    "",
                    "import type { Alert } from './item';\nexport const alert: Alert = { level: 'low' };\n",
                    "Level",
                ),
                "union closed" => (
                    "export type AppEvent = { type: 'start' } | { type: 'stop' };\n",
                    "export type AppEvent = { type: 'stop' };\n",
                    "import type { AppEvent } from './item';\nexport function handleEvent(event: AppEvent): string {\nif (event.type === 'start') return 'started';\nreturn event.type === 'stop' ? 'stopped' : 'unknown';\n}\n",
                    "import type { AppEvent } from './item';\nexport function handleEvent(event: AppEvent): string {\nreturn event.type === 'stop' ? 'stopped' : 'unknown';\n}\n",
                    "import { handleEvent } from './make';\nexport const state = handleEvent({ type: 'stop' });\n",
                    "AppEvent",
                ),
                _ => (
                    "export interface Item { id: number; }\n",
                    "export interface Item { id: number; label: string; }\n",
                    "import type { Item } from './item';\nexport function makeItem(): Item {\nreturn { id: 1 };\n}\nexport function makeDraft(): Item {\nreturn { id: 0 };\n}\n",
                    if case == "missing" {
                        "import type { Item } from './item';\nexport function makeItem(): Item {\nreturn { id: 1, label: 'first' };\n}\nexport function makeDraft(): Item {\nreturn { id: 0 };\n}\n"
                    } else if case == "asserted value" {
                        "import type { Item } from './item';\nexport function makeItem(): Item {\nreturn { id: 1, label: 'first' };\n}\nexport function makeDraft(): Item {\nreturn ({ id: 0, label: 'draft' } as Item);\n}\n"
                    } else {
                        "import type { Item } from './item';\nexport function makeItem(): Item {\nreturn { id: 1, label: 'first' };\n}\nexport function makeDraft(): Item {\nreturn { id: 0, label: 'draft' };\n}\n"
                    },
                    "import { makeItem, makeDraft } from './make';\nexport const items = [makeItem(), makeDraft()];\n",
                    "Item",
                ),
            };
            repo.write(&target, before);
            let producer = format!("src/make.{extension}");
            repo.write(&producer, producer_before);
            repo.write(format!("src/app.{extension}"), application);
            repo.init_git();
            repo.commit_all("initial");
            repo.write(&target, after);
            repo.write(&producer, producer_after);
            for staged in [false, true] {
                if staged {
                    repo.stage_all();
                    repo.write(&producer, "broken unstaged source");
                }
                let args = if staged {
                    vec!["--git", "--staged"]
                } else {
                    vec!["--git"]
                };
                let normal = repo.run_json("review", &args);
                let modified = normal["api_changes"]["modified"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|change| change["name"] == name);
                let blocking = matches!(
                    case,
                    "missing" | "asserted value" | "function" | "union added"
                );
                assert_eq!(
                    modified.is_some(),
                    blocking,
                    "{extension} {case} {staged}: {normal}"
                );
                let output = cargo_bin()
                    .args(["review", "--dir"])
                    .arg(repo.root())
                    .args(&args)
                    .arg("--hook")
                    .output()
                    .unwrap();
                let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(output.status.success(), !blocking, "{case}: {hook}");
                if let Some(change) = modified {
                    let entry = hook["api"]["mod"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|change| change["n"] == name)
                        .unwrap();
                    assert_eq!(entry["unverified"], change["unverified"], "{normal} {hook}");
                    let reason = if matches!(case, "missing" | "asserted value") {
                        "type_member_proof_failed"
                    } else if case == "function" {
                        "compatibility_unverified"
                    } else {
                        "unsupported_type_change"
                    };
                    assert_eq!(change["unverified"]["reason"], reason, "{normal}");
                    if matches!(case, "missing" | "asserted value") {
                        assert_eq!(
                            change["unverified"]["first_unproven"]["file"], producer,
                            "{normal}"
                        );
                        assert_eq!(
                            change["unverified"]["first_unproven"]["line"], 4,
                            "{normal}"
                        );
                        assert_eq!(
                            change["unverified"]["first_unproven"]["symbol"], "makeDraft",
                            "{normal}"
                        );
                        if !staged && case == "missing" {
                            let toon = cargo_bin()
                                .args(["review", "--dir"])
                                .arg(repo.root())
                                .args(["--git", "--format", "toon"])
                                .output()
                                .unwrap();
                            let text = String::from_utf8(toon.stdout).unwrap();
                            assert!(
                                text.contains("type_member_proof_failed")
                                    && text.contains("makeDraft"),
                                "{text}"
                            );
                        }
                    }
                }
            }
        }
    }
}
