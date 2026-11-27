//! TypeScript の interface へ必須プロパティを足し、値を作る箇所を同じ差分で直したときの
//! `review --git --hook` (Issue #89)。

use super::support::{TestRepo, cargo_bin};

#[test]
fn optional_type_members_verify_key_observers_and_module_augmentations() {
    let interface = "export interface Options { label: string; }";
    let alias = "export type Options = { label: string; };";
    let plain = "export const item: Options = {label:'a'};";
    let record = "export const keys: Record<keyof Options, number> = {label:1};";
    for (before, imported, usage, safe) in [
        (interface, "Options", record, false),
        (alias, "Options", record, false),
        (
            interface,
            "",
            "declare module './model' { interface Options { emphasis?: string } }\nexport {};",
            false,
        ),
        (
            alias,
            "Options",
            "const data={label:'a',emphasis:1}; export const item: Options = data;",
            false,
        ),
        (
            "export interface Options { label: Record<keyof this,string>; }",
            "Options",
            "export const item: Options = {label:{label:'a'}};",
            false,
        ),
        (alias, "Options", plain, true),
        (
            interface,
            "Options as Original",
            "interface Options { label: string }\nexport const keys: Record<keyof Options,number>={label:1};\nexport const item: Original={label:'a'};",
            true,
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let after = before.replace("; }", "; emphasis?: boolean; }");
        repo.write("model.ts", before);
        let import = if imported.is_empty() {
            String::new()
        } else {
            format!("import type {{ {imported} }} from './model';\n")
        };
        repo.write("reader.ts", format!("{import}{usage}\n"));
        repo.commit_all("base");
        repo.write("model.ts", after);
        repo.stage_all();
        for staged in [false, true] {
            if staged {
                repo.write("model.ts", before);
            }
            let mut cmd = cargo_bin();
            cmd.args(["review", "--dir"])
                .arg(repo.root())
                .args(["--git", "--hook"]);
            if staged {
                cmd.arg("--staged");
            }
            let output = cmd.output().unwrap();
            let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(
                output.status.success(),
                safe,
                "{before} {usage} staged={staged}: {hook}"
            );
            let bucket = if safe { "mod_compat" } else { "mod" };
            assert!(
                hook["api"][bucket]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["n"] == "Options")),
                "{hook}"
            );
        }
    }
}

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
fn typed_tuple_arrays_close_required_additions_with_unused_same_file_consumer() {
    let repo = TestRepo::new();
    let old_type = "export interface Flags {\n  red: boolean;\n  blue: boolean;\n}\n\
        export function countFlags(flags: Flags): number {\n  return Object.values(flags).filter(Boolean).length;\n}\n";
    let old_value = "{ red: true, blue: false }";
    let updated_value = "{ red: true, blue: false, green: true }";
    let table = |first, second| {
        format!(
            "import type {{ Flags }} from './flags';\n\
            const samples: Array<[Flags, number]> = [\n  [{first}, 1],\n  [{second}, 2],\n];\n\
            export const sampleCount = samples.length;\n"
        )
    };
    repo.write("flags.ts", old_type);
    repo.write("table.ts", table(old_value, old_value));
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "flags.ts",
        old_type.replace("  blue: boolean;", "  blue: boolean;\n  green: boolean;"),
    );

    for (source, expected) in [
        (table(updated_value, updated_value), 0),
        (table(updated_value, old_value), 1),
        (
            format!(
                "{}import {{ countFlags }} from './flags';\ncountFlags({old_value});\n",
                table(updated_value, updated_value)
            ),
            1,
        ),
        ("export const sampleCount = 0;\n".to_owned(), 1),
    ] {
        repo.write("table.ts", &source);
        let (code, json) = hook(&repo);
        let bucket = if expected == 0 { "mod_closed" } else { "mod" };
        assert_eq!(code, expected, "{source}: {json}");
        assert!(
            json["api"][bucket]
                .as_array()
                .is_some_and(|changes| changes.iter().any(|change| change["n"] == "Flags")),
            "{source}: {json}"
        );
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

/// `Partial<T>` の factory が既定値を並べた後ろに `...overrides` を置く形の 1 ケース。
struct PartialSpread {
    name: &'static str,
    /// factory.ts。`{TAGS}` に `, tags: <tags>` が入る。
    factory: String,
    /// 差分で足す `tags` の値。
    tags: &'static str,
    /// app.ts の変更後の呼び出し。変更前は `makeOptions();`。
    call: String,
    /// 変更の前後で置くほかのファイル (app.ts と違い、`makeOptions` を import しない)。
    files: Vec<(&'static str, String)>,
    closed: bool,
}

const PARTIAL_SPREAD_OPTIONS: &str =
    "export interface Options {\n readonly size: number;\n readonly label: string;\n}\n";
const PARTIAL_SPREAD_PARAMETER: &str = "overrides: Partial<Options> = {}";

/// 関数宣言の factory。
fn partial_spread_function(parameter: &str, body: &str) -> String {
    format!(
        "import type {{ Options }} from './options';\nexport function makeOptions({parameter}): Options {{\n {body}\n}}\n"
    )
}

/// `const` の矢印関数の factory。宣言の行が差分に入らないよう、本体は次の行から書く。
fn partial_spread_arrow(head: &str, parameter: &str, body: &str) -> String {
    format!(
        "import type {{ Options }} from './options';\n{head} = ({parameter}): Options => (\n {body}\n);\n"
    )
}

fn partial_spread_closed(name: &'static str, factory: String) -> PartialSpread {
    PartialSpread {
        name,
        factory,
        tags: "['x']",
        call: "makeOptions()".to_string(),
        files: Vec::new(),
        closed: true,
    }
}

fn partial_spread_blocking(name: &'static str, factory: String) -> PartialSpread {
    PartialSpread {
        closed: false,
        ..partial_spread_closed(name, factory)
    }
}

/// 変更前をコミットし、`Options` に `tags` を、factory の既定値に `tags` を足した差分を hook で
/// 確かめる。blocking なら、値を作る箇所の証明で止まったこと (理由) まで確かめる。
fn check_partial_spread(case: &PartialSpread) {
    let name = case.name;
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("options.ts", PARTIAL_SPREAD_OPTIONS);
    repo.write("factory.ts", case.factory.replace("{TAGS}", ""));
    repo.write(
        "app.ts",
        "import { makeOptions } from './factory';\nmakeOptions();\n",
    );
    for (path, text) in &case.files {
        repo.write(path, text);
    }
    repo.commit_all("initial");
    repo.write(
        "options.ts",
        PARTIAL_SPREAD_OPTIONS.replace("}\n", " readonly tags: readonly string[];\n}\n"),
    );
    repo.write(
        "factory.ts",
        case.factory
            .replace("{TAGS}", &format!(", tags: {}", case.tags)),
    );
    repo.write(
        "app.ts",
        format!(
            "import {{ makeOptions }} from './factory';\n{};\n",
            case.call
        ),
    );
    let (code, json) = hook(&repo);
    assert_eq!(code, if case.closed { 0 } else { 1 }, "{name}: {json}");
    let bucket = if case.closed { "mod_closed" } else { "mod" };
    let entry = json["api"][bucket]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["n"] == "Options"))
        .unwrap_or_else(|| panic!("{name}: {json}"));
    if !case.closed {
        assert_eq!(
            entry["unverified"]["reason"], "type_member_proof_failed",
            "{name}: {json}"
        );
    }
}

/// 既定値を並べた object literal で組み込み `Partial<T>` の引数を 1 つだけ spread する factory を、
/// 関数宣言と `const` の矢印関数で閉じる (Issue #463)。spread は追加したメンバーを宣言どおりの
/// 型でしか上書きせず、呼び出し側の実引数は `??` の形と同じく確かめる。
#[test]
fn partial_factories_close_partial_spreads() {
    let spread = "{ size: 1, label: 'a'{TAGS}, ...overrides }";
    let returned = format!("return {spread};");
    // 本体の中の束縛の名前に escape が無い対照 (escape のある方は blocking のテスト)。
    let plain_block = format!(" {{\n  const other = {{ tags: 1 }};\n  {returned}\n }}");
    // 文字列の escape は束縛にも呼び出しにも関わらない。
    let string_escape = format!("const marker = '{}u0061';\nexport function", '\\');
    // 呼び出し側の文字列の添字の escape は、解くと関係する名前にならない。
    let caller_string_escape = format!(
        "const labels: Record<string, number> = {{}};\nlabels['{b}u0061'];\nlabels['{b}x61'];\nmakeOptions({{ tags: ['y'] }})",
        b = '\\'
    );
    // 呼び出し側の無関係な escape の識別子は、追加した値の解決に関わらない。
    let caller_identifier_escape = format!("const n{}u006fte = 1;\nmakeOptions({{}})", '\\');
    let arrow = partial_spread_arrow("export const makeOptions", PARTIAL_SPREAD_PARAMETER, spread);
    let cases = [
        partial_spread_closed(
            "explicit value after the spread",
            partial_spread_function(
                PARTIAL_SPREAD_PARAMETER,
                "return { size: 1, label: 'a', ...overrides{TAGS} };",
            ),
        ),
        partial_spread_closed(
            "escape only in a string",
            partial_spread_function(PARTIAL_SPREAD_PARAMETER, &returned)
                .replace("export function", &string_escape),
        ),
        partial_spread_closed(
            "other parameter with a plain default",
            partial_spread_function(&format!("{PARTIAL_SPREAD_PARAMETER}, size = 1"), &returned),
        ),
        partial_spread_closed(
            "same-file caller passes tags",
            format!("{arrow}makeOptions({{ tags: ['y'] }});\n"),
        ),
        PartialSpread {
            call: "const item = 'y';\nmakeOptions({ tags: [item, item] })".to_string(),
            ..partial_spread_closed(
                "caller repeats one const in an array",
                partial_spread_function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call: caller_string_escape,
            ..partial_spread_closed(
                "caller with an escape only in a string key",
                partial_spread_function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call: caller_identifier_escape,
            ..partial_spread_closed(
                "caller with an unrelated escaped identifier passes no added value",
                partial_spread_function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        // `??` の形 (#228) の対照。呼び出し側の `__proto__` の負例と同じ factory。
        PartialSpread {
            tags: "overrides.tags ?? ['x']",
            call: "makeOptions({ tags: ['y'] })".to_string(),
            ..partial_spread_closed(
                "coalescing factory with a plain caller",
                partial_spread_function(
                    PARTIAL_SPREAD_PARAMETER,
                    "return { size: 1, label: 'a'{TAGS} };",
                ),
            )
        },
        partial_spread_closed(
            "function declaration",
            partial_spread_function(PARTIAL_SPREAD_PARAMETER, &returned),
        ),
        partial_spread_closed(
            "arrow with an expression body",
            partial_spread_arrow("export const makeOptions", PARTIAL_SPREAD_PARAMETER, spread),
        ),
        partial_spread_closed(
            "arrow with a block body",
            partial_spread_arrow(
                "export const makeOptions",
                PARTIAL_SPREAD_PARAMETER,
                &format!("{{\n {returned}\n}}"),
            )
            .replace("=> (\n", "=> ")
            .replace("\n);\n", ";\n"),
        ),
        partial_spread_closed(
            "binding without an escape in the body",
            partial_spread_function(PARTIAL_SPREAD_PARAMETER, &plain_block),
        ),
        PartialSpread {
            call: "makeOptions({})".to_string(),
            ..partial_spread_closed(
                "caller passes an empty object",
                partial_spread_function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call: "makeOptions({ tags: ['y'] })".to_string(),
            ..partial_spread_closed(
                "caller passes tags",
                partial_spread_arrow("export const makeOptions", PARTIAL_SPREAD_PARAMETER, spread),
            )
        },
    ];
    cases.iter().for_each(check_partial_spread);
}

/// 既定値の足し忘れ・spread の後ろの型の合わない上書き・`Partial<Options>` でない spread の元・
/// 入力を書き換えうる形・証明できない呼び出しや宣言の形は、blocking のまま残る。
#[test]
fn partial_factories_keep_unproven_partial_spreads_blocking() {
    let spread = "{ size: 1, label: 'a'{TAGS}, ...overrides }";
    let returned = format!("return {spread};");
    let function = |parameter: &str, body: &str| partial_spread_function(parameter, body);
    let arrow = partial_spread_arrow("export const makeOptions", PARTIAL_SPREAD_PARAMETER, spread);
    // `e` を Unicode escape で綴った名前で、`overrides` と同じ名前の束縛になる。
    let escaped = format!("overrid{}u0065s", '\\');
    let shadowed = format!(" {{\n  const {escaped} = {{ tags: 1 }};\n  {returned}\n }}");
    // `O` を Unicode escape で綴った呼び出しで、`refs` の名前の照合に現れない。
    let escaped_call = format!("make{}u004fptions({{ tags: undefined }})", '\\');
    // `i` を Unicode escape で綴った内側の束縛が、実引数の `items` を隠す。
    let escaped_binding_call = format!(
        "const items: readonly string[] = ['x'];\n{{\n const {}u0069tems: any = undefined;\n makeOptions({{ tags: items }});\n}}",
        '\\'
    );
    // `O` を `\x` で綴った文字列のキーで、名前空間から factory を取り出して呼ぶ。
    // 名前を字面に書かないファイルに置き、escape を解いて読む前段の判定まで確かめる。
    let escaped_key_call = format!(
        "import * as rows from './factory';\nrows['make{}x4fptions']({{ tags: undefined }});\n",
        '\\'
    );
    // 値の出どころの連鎖が深さの上限 (16) に届く。配列の要素の順によらず同じ判定になる。
    let chain: String = (1..15)
        .map(|index| format!("const a{index}: string = a{};\n", index - 1))
        .collect();
    let deep_chain =
        |elements: &str| format!("const a0 = 'y';\n{chain}makeOptions({{ tags: [{elements}] }})");
    let cases = [
        partial_spread_blocking(
            "arguments rewrites the input",
            function(
                PARTIAL_SPREAD_PARAMETER,
                &format!("for (arguments[0].tags of [undefined]) {{}}\n {returned}"),
            ),
        ),
        PartialSpread {
            files: vec![("rows.ts", escaped_key_call)],
            ..partial_spread_blocking(
                "escaped string key call through a namespace",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            tags: "overrides.tags ?? ['x']",
            call: "makeOptions({ __proto__: { tags: 1 } })".to_string(),
            ..partial_spread_blocking(
                "caller sets the prototype of the input",
                function(
                    PARTIAL_SPREAD_PARAMETER,
                    "return { size: 1, label: 'a'{TAGS} };",
                ),
            )
        },
        PartialSpread {
            call: deep_chain("a0, a14"),
            ..partial_spread_blocking(
                "chain at the depth limit after a short element",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call: deep_chain("a14, a0"),
            ..partial_spread_blocking(
                "chain at the depth limit before a short element",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        partial_spread_blocking(
            "missing default",
            function(
                PARTIAL_SPREAD_PARAMETER,
                "return { size: 1, label: 'a', ...overrides };",
            ),
        ),
        // 閉じる側の「explicit value after the spread」と同じ形で、足す値の型だけが違う。
        PartialSpread {
            tags: "1",
            ..partial_spread_blocking(
                "mismatched value after the spread",
                function(
                    PARTIAL_SPREAD_PARAMETER,
                    "return { size: 1, label: 'a', ...overrides{TAGS} };",
                ),
            )
        },
        PartialSpread {
            call: "const item: string = undefined as any;\nmakeOptions({ tags: [item] })"
                .to_string(),
            ..partial_spread_blocking(
                "caller hides any in an array element",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        partial_spread_blocking(
            "other parameter default reads a member",
            function(
                &format!("{PARTIAL_SPREAD_PARAMETER}, _ = defaults.flag"),
                &returned,
            )
            .replace(
                "export function",
                "const defaults = { flag: 0 };\nexport function",
            ),
        ),
        PartialSpread {
            call: escaped_binding_call,
            ..partial_spread_blocking(
                "escaped binding shadows the argument at the call site",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        partial_spread_blocking(
            "other parameter default rewrites the input",
            function(
                &format!(
                    "{PARTIAL_SPREAD_PARAMETER}, _ = Object.assign(overrides, {{ tags: undefined }})"
                ),
                &returned,
            ),
        ),
        partial_spread_blocking(
            "same-file caller passes undefined",
            format!("{arrow}makeOptions({{ tags: undefined }});\n"),
        ),
        PartialSpread {
            call: "const value: any = undefined;\nmakeOptions({ tags: value })".to_string(),
            ..partial_spread_blocking(
                "caller passes an any value",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call:
                "const value: readonly string[] = undefined as any;\nmakeOptions({ tags: value })"
                    .to_string(),
            ..partial_spread_blocking(
                "caller hides any behind an annotation",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call: escaped_call,
            ..partial_spread_blocking(
                "escaped call in another file",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        partial_spread_blocking(
            "spread source is not Partial<Options>",
            function("overrides: { size?: number } = {}", &returned),
        ),
        partial_spread_blocking(
            "Partial of another type",
            function("overrides: Partial<Other> = {}", &returned).replace(
                "export function",
                "interface Other { readonly tags?: number }\nexport function",
            ),
        ),
        partial_spread_blocking(
            "parameter without a default",
            function("overrides: Partial<Options>", &returned),
        ),
        partial_spread_blocking(
            "returned through a variable",
            function(
                PARTIAL_SPREAD_PARAMETER,
                "const merged = { size: 1, label: 'a'{TAGS}, ...overrides };\n return merged;",
            ),
        ),
        partial_spread_blocking(
            "escaped binding in the body",
            function(PARTIAL_SPREAD_PARAMETER, &shadowed),
        ),
        partial_spread_blocking(
            "let arrow",
            partial_spread_arrow("export let makeOptions", PARTIAL_SPREAD_PARAMETER, spread),
        ),
        partial_spread_blocking(
            "const with a type annotation",
            partial_spread_arrow(
                "export const makeOptions: () => unknown",
                PARTIAL_SPREAD_PARAMETER,
                spread,
            ),
        ),
        PartialSpread {
            call: "makeOptions({ tags: undefined })".to_string(),
            ..partial_spread_blocking(
                "caller passes undefined",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call: "const input = { size: 2 };\nmakeOptions(input)".to_string(),
            ..partial_spread_blocking(
                "caller passes a variable",
                function(PARTIAL_SPREAD_PARAMETER, &returned),
            )
        },
        PartialSpread {
            call: "makeOptions.apply(null, [])".to_string(),
            ..partial_spread_blocking(
                "factory called through apply",
                partial_spread_arrow("export const makeOptions", PARTIAL_SPREAD_PARAMETER, spread),
            )
        },
    ];
    cases.iter().for_each(check_partial_spread);
}

/// object literal の `__proto__: 値` はプロトタイプを決めるだけで、自分のプロパティを作らない。
/// 必須の `__proto__` を足しても、その書き方の値では閉じない。短縮形 (`{ __proto__ }`) は自分の
/// プロパティを作るので閉じる (対照)。
#[test]
fn required_additions_do_not_count_proto_setters() {
    let options = "export interface Options {\n readonly size: number;\n}\n";
    let factory = |value: &str| {
        format!(
            "import type {{ Options }} from './options';\nexport function makeOptions(): Options {{\n const __proto__ = 1;\n return {value};\n}}\n"
        )
    };
    for (case, value, closed) in [
        ("proto setter", "{ size: 1, __proto__: 1 }", false),
        ("shorthand proto property", "{ size: 1, __proto__ }", true),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("options.ts", options);
        repo.write("factory.ts", factory("{ size: 1 }"));
        repo.commit_all("initial");
        repo.write(
            "options.ts",
            options.replace("}\n", " readonly __proto__: number;\n}\n"),
        );
        repo.write("factory.ts", factory(value));
        let (code, json) = hook(&repo);
        assert_eq!(code, if closed { 0 } else { 1 }, "{case}: {json}");
        let bucket = if closed { "mod_closed" } else { "mod" };
        assert!(
            json["api"][bucket]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == "Options")),
            "{case}: {json}"
        );
    }
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
fn required_callbacks_close_only_when_each_producer_provides_a_callable() {
    let repo = TestRepo::new();
    repo.init_git();
    let options = "export interface Options { readonly size: number; }\n";
    let defaults = "import type { Options } from './options';\n\
        const callback = () => {};\n\
        function namedCallback() {}\n\
        export function defaults(): Options {\n return { size: 1 };\n}\n\
        export function draft(): Options {\n return { size: 2 };\n}\n";
    repo.write("options.ts", options);
    repo.write("defaults.ts", defaults);
    repo.write(
        "app.ts",
        "import { defaults, draft } from './defaults';\nexport const options = [defaults(), draft()];\n",
    );
    repo.commit_all("initial");

    for signature in ["reset(): void", "readonly reset: () => void"] {
        repo.write(
            "options.ts",
            options.replace("; }", &format!("; {signature}; }}")),
        );
        for (second, expected) in [
            (", reset() {}", 0),
            (", reset: callback", 0),
            (", reset: namedCallback", 0),
            ("", 1),
            (", reset: 0", 1),
            (", reset: callback as any", 1),
            (", get reset() { return 0; }", 1),
            (", reset: (required: number) => {}", 1),
        ] {
            repo.write(
                "defaults.ts",
                defaults
                    .replace("size: 1", "size: 1, reset: () => {}")
                    .replace("size: 2", &format!("size: 2{second}")),
            );
            let (code, json) = hook(&repo);
            let bucket = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{signature}/{second}: {json}");
            assert_eq!(json["api"][bucket][0]["n"], "Options", "{json}");
        }
        for (declaration, expected) in [
            ("const reset = () => {};", 0),
            ("const reset: () => void = () => {};", 0),
            ("function reset() {}", 0),
            ("function reset() {}\nreset = 0 as any;", 1),
        ] {
            repo.write(
                "defaults.ts",
                defaults
                    .replace("const callback = () => {};", declaration)
                    .replace("size: 1", "size: 1, reset")
                    .replace("size: 2", "size: 2, reset"),
            );
            let (code, json) = hook(&repo);
            let bucket = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{signature}/{declaration}: {json}");
            assert_eq!(json["api"][bucket][0]["n"], "Options", "{json}");
        }
    }
    repo.write(
        "options.ts",
        options.replace("; }", "; reset(): void; reset(required: number): void; }"),
    );
    repo.write(
        "defaults.ts",
        defaults
            .replace("size: 1", "size: 1, reset() {}")
            .replace("size: 2", "size: 2, reset() {}"),
    );
    let (code, json) = hook(&repo);
    assert_eq!(code, 1, "{json}");
    assert_eq!(json["api"]["mod"][0]["n"], "Options", "{json}");
}

#[test]
fn replaced_properties_close_only_after_all_values_and_uses_follow() {
    let repo = TestRepo::new();
    repo.init_git();
    let options = "export interface Options { readonly size: number; readonly label: string; }\n";
    let defaults = "import type { Options } from './options';\n\
        export function defaults(): Options {\n return {\n size: 1,\n label: 'a',\n };\n}\n\
        export function draft(): Options {\n return { size: 2, label: 'b' };\n}\n";
    let view = "import type { Options } from './options';\n\
        export function describe(options: Options): string {\n return options.label;\n}\n";
    repo.write("options.ts", options);
    repo.write("defaults.ts", defaults);
    repo.write("view.ts", view);
    repo.write(
        "app.ts",
        "import { defaults, draft } from './defaults';\n\
         import { describe } from './view';\n\
         export const descriptions = [describe(defaults()), describe(draft())];\n",
    );
    repo.commit_all("initial");
    repo.write("options.ts", options.replace("label", "title"));
    for (draft, read, expected) in [
        (", title: 'b'", "title", 0),
        ("", "title", 1),
        (", label: 'b', title: 'b'", "title", 1),
        (", title: 42", "title", 1),
        (", title: 'b'", "label", 1),
    ] {
        repo.write(
            "defaults.ts",
            defaults
                .replace("label: 'a'", "title: 'a'")
                .replace(", label: 'b'", draft),
        );
        repo.write(
            "view.ts",
            view.replace("options.label", &format!("options.{read}")),
        );
        let (code, json) = hook(&repo);
        let bucket = if expected == 0 { "mod_closed" } else { "mod" };
        assert_eq!(code, expected, "{draft}/{read}: {json}");
        assert_eq!(json["api"][bucket][0]["n"], "Options", "{json}");
    }
    repo.write("defaults.ts", defaults.replace("label:", "title:"));
    repo.write("view.ts", view.replace("options.label", "options.title"));
    repo.write(
        "options.ts",
        options.replace("label", "title").replace("number", "1 | 2"),
    );
    let (code, json) = hook(&repo);
    assert_eq!(code, 1, "{json}");
    assert_eq!(json["api"]["mod"][0]["n"], "Options", "{json}");
}

#[test]
fn readonly_array_properties_check_all_producers_and_builtin_binding() {
    let repo = TestRepo::new();
    repo.init_git();
    let options = "export interface Options { readonly size: number; }\n";
    let defaults = "import type { Options } from './options';\n\
        export function defaults(): Options {\n return { size: 1 };\n}\n\
        export function draft(input: readonly string[]): Options {\n return { size: 2 };\n}\n";
    repo.write("options.ts", options);
    repo.write("defaults.ts", defaults);
    repo.write("ambient.d.ts", "// ambient types\n");
    repo.write(
        "app.ts",
        "import { defaults, draft } from './defaults';\nexport const options = [defaults(), draft(['b'])];\n",
    );
    repo.commit_all("initial");

    for annotation in ["readonly string[]", "ReadonlyArray<string>", "string[]"] {
        repo.write(
            "options.ts",
            options.replace("; }", &format!("; readonly tags: {annotation}; }}")),
        );
        for (draft, expected) in [
            (", tags: ['b']", 0),
            ("", 1),
            (", tags: [1]", 1),
            (", tags: input", i32::from(annotation == "string[]")),
        ] {
            repo.write(
                "defaults.ts",
                defaults
                    .replace("size: 1", "size: 1, tags: ['a']")
                    .replace("size: 2", &format!("size: 2{draft}")),
            );
            let (code, json) = hook(&repo);
            let bucket = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{annotation}/{draft}: {json}");
            assert_eq!(json["api"][bucket][0]["n"], "Options", "{json}");
        }
    }

    let added = options.replace("; }", "; readonly tags: ReadonlyArray<string>; }");
    repo.write(
        "defaults.ts",
        defaults
            .replace("size: 1", "size: 1, tags: ['a']")
            .replace("size: 2", "size: 2, tags: ['b']"),
    );
    repo.write(
        "options.ts",
        format!("type ReadonlyArray<T> = {{ value: T }};\n{added}"),
    );
    let (code, json) = hook(&repo);
    assert_eq!(code, 1, "{json}");
    assert_eq!(json["api"]["mod"][0]["n"], "Options", "{json}");

    repo.write("options.ts", added);
    repo.write(
        "ambient.d.ts",
        "interface ReadonlyArray<T> { requiredExtra: T }\n",
    );
    let (code, json) = hook(&repo);
    assert_eq!(code, 1, "{json}");
    assert!(
        json["api"]["mod"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| change["n"] == "Options")),
        "{json}"
    );
}

/// 型注釈付きの必須引数のプロパティの読み出し (`source.tags`) の 1 ケース。
#[derive(Default)]
struct ParameterRead {
    name: &'static str,
    /// `Options` に足す `tags` の型。
    member: &'static str,
    /// options.ts と defaults.ts のほかに置くファイル。
    files: Vec<(&'static str, &'static str)>,
    /// defaults.ts。`{TAGS}` に `tags` のプロパティが入る。
    defaults: String,
    /// `tags` の値。
    value: &'static str,
    closed: bool,
}

const PARAMETER_READ_OPTIONS: &str =
    "export interface Options {\n readonly size: number;\n readonly label: string;\n}\n";
const PARAMETER_READ_INLINE: &str = "{ readonly size: number; readonly tags: readonly string[] }";
const PARAMETER_READ_IMPORT: &str = "import type { Source } from './source';\n";
const PARAMETER_READ_SOURCE: &str =
    "export interface Source { readonly size: number; readonly tags: readonly string[] }\n";
const PARAMETER_READ_NUMBER_SOURCE: &str =
    "export interface Source { readonly size: number; readonly tags: number }\n";

/// `source` を受けて `Options` を返す関数。
fn parameter_read_defaults(prelude: &str, parameter: &str) -> String {
    format!(
        "import type {{ Options }} from './options';\n{prelude}export function defaults(source: {parameter}): Options {{\n return {{ size: source.size, label: 'a'{{TAGS}} }};\n}}\n"
    )
}

/// インラインの object 型の引数から `source.tags` を `readonly string[]` に渡し、閉じるケース。
fn parameter_read_closed(name: &'static str) -> ParameterRead {
    ParameterRead {
        name,
        member: "readonly string[]",
        defaults: parameter_read_defaults("", PARAMETER_READ_INLINE),
        value: "source.tags",
        closed: true,
        ..ParameterRead::default()
    }
}

fn parameter_read_blocking(name: &'static str) -> ParameterRead {
    ParameterRead {
        closed: false,
        ..parameter_read_closed(name)
    }
}

/// 変更前をコミットし、`Options` に `tags` を、`defaults` の戻り値に `tags` を足した差分を
/// hook で確かめる。blocking なら、値を作る箇所の証明で止まったこと (理由) まで確かめる。
fn check_parameter_read(case: &ParameterRead) {
    check_parameter_read_with(case, |_| {});
}

/// [`check_parameter_read`] で、ファイルを置いた後・コミットの前に `setup` を呼ぶ。
fn check_parameter_read_with(case: &ParameterRead, setup: impl FnOnce(&TestRepo)) {
    let name = case.name;
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("options.ts", PARAMETER_READ_OPTIONS);
    repo.write("defaults.ts", case.defaults.replace("{TAGS}", ""));
    for (path, text) in &case.files {
        if let Some((parent, _)) = path.rsplit_once('/') {
            repo.create_dir_all(parent);
        }
        repo.write(path, text);
    }
    setup(&repo);
    repo.commit_all("initial");
    repo.write(
        "options.ts",
        PARAMETER_READ_OPTIONS.replace("}\n", &format!(" readonly tags: {};\n}}\n", case.member)),
    );
    repo.write(
        "defaults.ts",
        case.defaults
            .replace("{TAGS}", &format!(", tags: {}", case.value)),
    );
    let (code, json) = hook(&repo);
    assert_eq!(code, if case.closed { 0 } else { 1 }, "{name}: {json}");
    let bucket = if case.closed { "mod_closed" } else { "mod" };
    let entry = json["api"][bucket]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["n"] == "Options"))
        .unwrap_or_else(|| panic!("{name}: {json}"));
    if !case.closed {
        assert_eq!(
            entry["unverified"]["reason"], "type_member_proof_failed",
            "{name}: {json}"
        );
    }
}

/// 型注釈付きの必須引数のプロパティの読み出し (`source.tags`) を、宣言された型で型付けする。
/// 注釈はインラインの object 型か、ファイル直下の interface・object 型の別名 (相対 import を含む)。
#[test]
fn typed_parameter_reads_close_declared_member_types() {
    let cases = [
        // Issue の 2 例。optional のメンバーには undefined が足される。
        parameter_read_closed("inline object type"),
        ParameterRead {
            member: "string[] | undefined",
            files: vec![(
                "source.ts",
                "export interface Source { readonly size: number; readonly tags?: string[] }\n",
            )],
            defaults: parameter_read_defaults(PARAMETER_READ_IMPORT, "Source"),
            ..parameter_read_closed("imported interface with an optional member")
        },
        ParameterRead {
            defaults: parameter_read_defaults(
                "type Source = { readonly size: number; readonly tags: readonly string[] };\n",
                "Source",
            ),
            ..parameter_read_closed("type alias in the same file")
        },
    ];
    cases.iter().for_each(check_parameter_read);
}

/// 宣言された型が追加したメンバーの型に合わない・型を決められない読み出しは、blocking のまま。
#[test]
fn typed_parameter_reads_keep_unproven_types_blocking() {
    let inline = |parameter: &str| parameter_read_defaults("", parameter);
    let cases = [
        ParameterRead {
            files: vec![(
                "source.ts",
                "export interface Source { readonly size: number; readonly tags?: readonly string[] }\n",
            )],
            defaults: parameter_read_defaults(PARAMETER_READ_IMPORT, "Source"),
            ..parameter_read_blocking("optional member to a required type")
        },
        ParameterRead {
            value: "source.size",
            ..parameter_read_blocking("number to an array")
        },
        ParameterRead {
            member: "string[]",
            ..parameter_read_blocking("readonly array to a mutable array")
        },
        ParameterRead {
            defaults: inline("{ readonly size: number; readonly tags: any[] }"),
            ..parameter_read_blocking("any elements")
        },
        ParameterRead {
            defaults: inline("Missing"),
            ..parameter_read_blocking("unresolved type")
        },
        ParameterRead {
            defaults: parameter_read_defaults(
                "interface Base { readonly size: number }\ninterface Source extends Base { readonly tags: readonly string[] }\n",
                "Source",
            ),
            ..parameter_read_blocking("interface with extends")
        },
        ParameterRead {
            defaults: inline(
                "{ readonly size: number; readonly [key: string]: readonly string[] | number }",
            ),
            ..parameter_read_blocking("index signature only")
        },
        ParameterRead {
            defaults: inline("{ readonly size: number; get tags(): readonly string[] }"),
            ..parameter_read_blocking("getter in the type")
        },
        ParameterRead {
            defaults: parameter_read_defaults(
                "interface Source { readonly size: number; readonly tags: readonly string[] }\ninterface Source { readonly label: string }\n",
                "Source",
            ),
            ..parameter_read_blocking("merged interface")
        },
        ParameterRead {
            defaults: inline(
                "{ readonly size: number; readonly tags: readonly string[]; readonly \"tags\": readonly string[] }",
            ),
            ..parameter_read_blocking("quoted duplicate key")
        },
        ParameterRead {
            defaults: inline("{ readonly size: number; readonly toString: readonly string[] }"),
            value: "source.toString",
            ..parameter_read_blocking("name inherited from Object.prototype")
        },
        ParameterRead {
            value: "source?.tags",
            ..parameter_read_blocking("optional chain")
        },
        ParameterRead {
            value: "source[\"tags\"]",
            ..parameter_read_blocking("bracket access")
        },
        ParameterRead {
            defaults: inline(
                "{ readonly size: number; readonly inner: { readonly tags: readonly string[] } }",
            ),
            value: "source.inner.tags",
            ..parameter_read_blocking("two levels")
        },
    ];
    cases.iter().for_each(check_parameter_read);
}

/// 型の名前が内側の型引数やクラス式の名前に、引数の名前がクラス式の名前や escape で綴った束縛に
/// 隠れるときは、外側の宣言で型付けしない。外側の宣言を互換、実際に使われる側を非互換にし、
/// 対照は原因だけを取り除いたもの。
#[test]
fn typed_parameter_reads_do_not_fall_back_to_shadowed_declarations() {
    let outer = "interface Source { readonly size: number; readonly tags: readonly string[] }\n";
    let function = |parameters: &str| {
        format!(
            "import type {{ Options }} from './options';\n{outer}export function defaults{parameters}(source: Source): Options {{\n return {{ size: source.size, label: 'a'{{TAGS}} }};\n}}\n"
        )
    };
    let class = |parameters: &str| {
        format!(
            "import type {{ Options }} from './options';\n{outer}export class Builder{parameters} {{\n build(source: Source): number {{\n  const result: Options = {{ size: source.size, label: 'a'{{TAGS}} }};\n  return result.size;\n }}\n}}\n"
        )
    };
    let inner = "<Source extends { readonly size: number; readonly tags: number }>";
    let renamed = "<T extends { readonly size: number; readonly tags: number }>";
    // クラス式の中では、その名前がクラス自身 (型と値) を指す。export した const の初期化子を
    // 変えると const 自体の変更にもなるので、export しない。
    let class_expression = |name: &str| {
        format!(
            "import type {{ Options }} from './options';\n{outer}const Builder = class {name} {{\n readonly size = 1;\n readonly tags = 42;\n build(source: Source): number {{\n  const result: Options = {{ size: source.size, label: 'a'{{TAGS}} }};\n  return result.size;\n }}\n}};\n"
        )
    };
    let class_value = |name: &str| {
        format!(
            "import type {{ Options }} from './options';\nexport function defaults(source: {PARAMETER_READ_INLINE}): Options {{\n const Reader = class {name} {{\n  static readonly tags = 42;\n  static build(): number {{\n   const result: Options = {{ size: 1, label: 'a'{{TAGS}} }};\n   return result.size;\n  }}\n }};\n return {{ size: Reader.build(), label: 'a'{{TAGS}} }};\n}}\n"
        )
    };
    let block = |binding: &str| {
        format!(
            "import type {{ Options }} from './options';\nexport function defaults(source: {PARAMETER_READ_INLINE}): Options {{\n {{\n  const {binding} = {{ size: 1, tags: 42 }};\n  return {{ size: source.size, label: 'a'{{TAGS}} }};\n }}\n}}\n"
        )
    };
    // `o` を Unicode escape で綴った名前で、`source` と同じ名前の束縛になる。
    let escaped = format!("s{}u006furce", '\\');
    let cases = [
        ParameterRead {
            defaults: function(inner),
            ..parameter_read_blocking("function type parameter")
        },
        ParameterRead {
            defaults: function(renamed),
            ..parameter_read_closed("function type parameter renamed")
        },
        ParameterRead {
            defaults: class(inner),
            ..parameter_read_blocking("class type parameter")
        },
        ParameterRead {
            defaults: class(renamed),
            ..parameter_read_closed("class type parameter renamed")
        },
        ParameterRead {
            defaults: class_expression("Source"),
            ..parameter_read_blocking("class expression named like the type")
        },
        ParameterRead {
            defaults: class_expression("Named"),
            ..parameter_read_closed("class expression with another name")
        },
        ParameterRead {
            defaults: class_value("source"),
            ..parameter_read_blocking("class expression named like the parameter")
        },
        ParameterRead {
            defaults: class_value("Named"),
            ..parameter_read_closed("class expression beside the parameter")
        },
        ParameterRead {
            defaults: block(&escaped),
            ..parameter_read_blocking("escaped binding")
        },
        ParameterRead {
            defaults: block("other"),
            ..parameter_read_closed("plain binding")
        },
    ];
    cases.iter().for_each(check_parameter_read);
}

/// import の先が、package.json・継いだ設定・`references` の先の設定で別のファイルに解けうるとき、
/// 宣言したファイルに同じ名前の別の書き出しがあるときは、宣言で型付けしない。対照は原因だけを
/// 取り除いたもの。
#[test]
fn typed_parameter_reads_do_not_use_redirected_imports() {
    let directory = vec![
        ("source/index.ts", PARAMETER_READ_SOURCE),
        ("source/actual.d.ts", PARAMETER_READ_NUMBER_SOURCE),
    ];
    let manifest = [
        directory.clone(),
        vec![("source/package.json", "{ \"types\": \"./actual.d.ts\" }\n")],
    ]
    .concat();
    let suffixed = |base: &'static str| {
        vec![
            ("source.ts", PARAMETER_READ_SOURCE),
            ("source.native.ts", PARAMETER_READ_NUMBER_SOURCE),
            ("tsconfig.json", "{ \"extends\": \"./base.json\" }\n"),
            ("base.json", base),
        ]
    };
    // ルートの tsconfig.json は `references` の先のプロジェクトに任せる (先の設定がファイルに効く)。
    let referenced = |app: &'static str| {
        vec![
            ("source.ts", PARAMETER_READ_SOURCE),
            ("source.native.ts", PARAMETER_READ_NUMBER_SOURCE),
            (
                "tsconfig.json",
                "{ \"files\": [], \"references\": [{ \"path\": \"./config/app.json\" }] }\n",
            ),
            ("config/app.json", app),
        ]
    };
    // 同じ名前の書き出しが別にあると、import の先が直接の宣言に決まらない (どれも型エラー)。
    let exported_twice = |other: &'static str| {
        vec![
            ("source.ts", other),
            (
                "other.ts",
                "export interface Other { readonly size: number; readonly tags: number }\n",
            ),
        ]
    };
    let imported = parameter_read_defaults(PARAMETER_READ_IMPORT, "Source");
    let cases = [
        ParameterRead {
            files: vec![("source.ts", PARAMETER_READ_SOURCE)],
            defaults: imported.clone(),
            ..parameter_read_closed("plain import")
        },
        ParameterRead {
            files: exported_twice(
                "export interface Source { readonly size: number; readonly tags: readonly string[] }\nexport { Other as \"Source\" } from './other';\n",
            ),
            defaults: imported.clone(),
            ..parameter_read_blocking("quoted re-export of the name")
        },
        ParameterRead {
            files: exported_twice(
                "export interface Source { readonly size: number; readonly tags: readonly string[] }\nexport * as \"Source\" from './other';\n",
            ),
            defaults: imported.clone(),
            ..parameter_read_blocking("quoted namespace export of the name")
        },
        ParameterRead {
            files: exported_twice(
                "export interface Source { readonly size: number; readonly tags: readonly string[] }\nconst value = 1;\nexport = value;\n",
            ),
            defaults: imported.clone(),
            ..parameter_read_blocking("export assignment")
        },
        ParameterRead {
            files: exported_twice(
                "export interface Source { readonly size: number; readonly tags: readonly string[] }\nexport default interface Source { readonly label: string }\n",
            ),
            defaults: imported.clone(),
            ..parameter_read_blocking("default interface of the same name")
        },
        ParameterRead {
            files: manifest,
            defaults: imported.clone(),
            ..parameter_read_blocking("package.json in the directory")
        },
        ParameterRead {
            files: directory,
            defaults: imported.clone(),
            ..parameter_read_closed("directory without package.json")
        },
        ParameterRead {
            files: suffixed(
                "{ \"compilerOptions\": { \"moduleSuffixes\": [\".native\", \"\"] } }\n",
            ),
            defaults: imported.clone(),
            ..parameter_read_blocking("moduleSuffixes through extends")
        },
        ParameterRead {
            files: suffixed("{ \"compilerOptions\": { \"strict\": true } }\n"),
            defaults: imported.clone(),
            ..parameter_read_closed("extends without moduleSuffixes")
        },
        ParameterRead {
            files: referenced(
                "{ \"include\": [\"../*.ts\"], \"compilerOptions\": { \"moduleSuffixes\": [\".native\", \"\"] } }\n",
            ),
            defaults: imported.clone(),
            ..parameter_read_blocking("moduleSuffixes in a referenced project")
        },
        ParameterRead {
            files: referenced("{ \"include\": [\"../*.ts\"] }\n"),
            defaults: imported,
            ..parameter_read_closed("referenced project without moduleSuffixes")
        },
    ];
    cases.iter().for_each(check_parameter_read);
}

/// `extends` の先が解析 root の中の symlink なら、読めない設定として型付けしない。対照は
/// `typed_parameter_reads_do_not_use_redirected_imports` の「extends without moduleSuffixes」
/// (同じ設定を通常のファイルで置いたもの)。
#[test]
#[cfg(unix)]
fn typed_parameter_reads_do_not_trust_symlinked_configs() {
    let case = ParameterRead {
        files: vec![
            ("source.ts", PARAMETER_READ_SOURCE),
            ("source.native.ts", PARAMETER_READ_NUMBER_SOURCE),
            ("tsconfig.json", "{ \"extends\": \"./base.json\" }\n"),
            (
                "shared/base.json",
                "{ \"compilerOptions\": { \"moduleSuffixes\": [\".native\", \"\"] } }\n",
            ),
        ],
        defaults: parameter_read_defaults(PARAMETER_READ_IMPORT, "Source"),
        ..parameter_read_blocking("symlinked base config")
    };
    check_parameter_read_with(&case, |repo| {
        std::os::unix::fs::symlink("shared/base.json", repo.path("base.json")).unwrap();
    });
}

/// `readonly` は union の先頭の配列型だけに掛かる (`readonly string[] | undefined` は
/// `(readonly string[]) | undefined`)。必須プロパティの追加と、残るメンバーの削除の両方で証明する。
#[test]
fn readonly_array_unions_close_required_additions_and_removals() {
    let options =
        "export interface Options {\n readonly size: number;\n readonly label: string;\n}\n";
    let defaults = "import type { Options } from './options';\n\
        export function defaults(): Options {\n return { size: 1, label: 'a' };\n}\n";
    for annotation in [
        "readonly string[] | undefined",
        "readonly string[] | null",
        "readonly string[] | undefined | null",
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("options.ts", options);
        repo.write("defaults.ts", defaults);
        repo.commit_all("initial");
        let added = options.replace("}\n", &format!(" readonly tags: {annotation};\n}}\n"));
        repo.write("options.ts", &added);
        for (value, expected) in [(", tags: ['x']", 0), ("", 1), (", tags: [1]", 1)] {
            repo.write(
                "defaults.ts",
                defaults.replace("label: 'a'", &format!("label: 'a'{value}")),
            );
            let (code, json) = hook(&repo);
            let bucket = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{annotation}/{value}: {json}");
            assert_eq!(json["api"][bucket][0]["n"], "Options", "{json}");
        }

        // 残る `tags` の型を読めれば、`label` の削除も同じ差分の値の直しで閉じる。
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("options.ts", &added);
        repo.write(
            "defaults.ts",
            defaults.replace("label: 'a'", "label: 'a', tags: ['x']"),
        );
        repo.commit_all("initial");
        repo.write(
            "options.ts",
            added.replace(" readonly label: string;\n", ""),
        );
        for (value, expected) in [
            ("tags: ['x']", 0),
            ("label: 'a', tags: ['x']", 1),
            // 残る配列のメンバーは値の不変性で証明するので、型の合う値でも変えれば閉じない。
            ("tags: ['y']", 1),
            ("tags: [1]", 1),
        ] {
            repo.write("defaults.ts", defaults.replace("label: 'a'", value));
            let (code, json) = hook(&repo);
            let bucket = if expected == 0 { "mod_closed" } else { "mod" };
            assert_eq!(code, expected, "{annotation}/removal {value}: {json}");
            assert_eq!(json["api"][bucket][0]["n"], "Options", "{json}");
        }
    }
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
