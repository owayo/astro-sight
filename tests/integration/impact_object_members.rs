//! 静的オブジェクトの未変更メンバーと保守的な影響判定の対照。

use super::support::*;

fn object_repo(extension: &str, before: &str, after: &str, consumer: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.write(format!("layout.{extension}"), before);
    repo.write(
        format!("consumer.{extension}"),
        format!("import {{ recordLayout }} from './layout';\n{consumer}\n"),
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(format!("layout.{extension}"), after);
    repo
}

fn assert_object_impact(repo: &TestRepo, expected_blocking: bool) {
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("impact");
    assert_eq!(
        !output.status.success(),
        expected_blocking,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert_eq!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|caller| caller["line"] == 1)),
        expected_blocking,
        "{context}"
    );
    if !expected_blocking {
        assert!(
            change["informational_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["line"] == 1 && caller["confidence"] == "informational"),
            "{context}"
        );
    }
}

#[test]
fn impact_object_members_keep_reads_checked_against_type_queries_blocking() {
    let mut failures = Vec::new();
    for (query, old_value, new_value, suffix, blocking) in [
        ("typeof recordLayout.other", "'a'", "1", "", true),
        ("typeof recordLayout.other", "'a'", "'b'", " as const", true),
        ("(typeof recordLayout)['other']", "'a'", "1", "", true),
        ("typeof recordLayout", "'a'", "1", "", true),
        ("keyof typeof recordLayout", "'a'", "", " as const", true),
        ("LocalKeys", "'a'", "", "", true),
        ("typeof alias.other", "'a'", "1", "", true),
        ("typeof lib.recordLayout.other", "'a'", "1", "", true),
        (
            "typeof import('./layout').recordLayout.other",
            "'a'",
            "1",
            "",
            true,
        ),
        ("typeof foreign.other", "'a'", "1", "", false),
        ("typeof bucket.recordLayout.other", "'a'", "1", "", true),
        ("typeof viaDefault.other", "'a'", "1", "", true),
        ("typeof direct.other", "'a'", "1", "", true),
        ("typeof extracted", "'a'", "1", "", true),
        ("typeof recordLayout.other", "'a'", "'b'", "", false),
        ("typeof recordLayout.label", "'a'", "1", "", false),
        ("string", "'a'", "1", "", false),
    ] {
        let repo = TestRepo::new();
        let declaration = |value: &str| {
            let label = if query == "keyof typeof recordLayout" {
                "other"
            } else {
                "a"
            };
            let prop = if value.is_empty() {
                String::new()
            } else {
                format!(", other: {value}")
            };
            let local = if query == "LocalKeys" {
                "export type Out = keyof typeof recordLayout extends 'label' ? number : string;\n"
            } else {
                ""
            };
            format!("export const recordLayout = {{ label: '{label}'{prop} }}{suffix};\n{local}")
        };
        repo.write("layout.ts", declaration(old_value));
        repo.write("foreign.ts", "export const foreign = {other: 'a'};\n");
        repo.write("barrel.ts", "export * as bucket from './layout';\nexport {recordLayout as default} from './layout';\n");
        let result_type = if query == "typeof recordLayout" {
            "Query['other']"
        } else {
            "Query"
        };
        let extra = match query {
            "typeof bucket.recordLayout.other" | "typeof viaDefault.other" => {
                "import viaDefault, {bucket} from './barrel';"
            }
            "LocalKeys" => "import type {Out as LocalKeys} from './layout';",
            "typeof direct.other" => "const direct = recordLayout;",
            "typeof extracted" => "const {other: extracted} = recordLayout;",
            "string" => "export const category = typeof recordLayout.other;",
            _ => "",
        };
        repo.write("types.ts", format!("import {{ recordLayout, recordLayout as alias }} from './layout';\nimport * as lib from './layout';\nimport {{ foreign }} from './foreign';\n{extra}\ntype Query = {query};\nexport type Out = {result_type};\n"));
        repo.write("use.tsx", "import { recordLayout } from './layout';\nimport type { Out } from './types';\nexport function read(): Out {\n  return recordLayout.label;\n}\n");
        repo.init_git();
        repo.commit_all("base");
        repo.write("layout.ts", declaration(new_value));
        repo.stage_all();
        for staged in [false, true] {
            if staged {
                repo.write("layout.ts", declaration(old_value));
                repo.write("types.ts", "export type Out = string;\n");
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
            let contains_read = |category: &str| {
                hook[category]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|change| change["refs"].as_array().unwrap())
                    .any(|entry| entry["p"] == "use.tsx" && entry["ln"] == 3)
            };
            if contains_read("impacts") != blocking || contains_read("impact_info") == blocking {
                failures.push(format!("{query}, {suffix:?}, staged={staged}: {hook}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn impact_object_members_only_unchanged_direct_reads_are_informational() {
    for extension in ["js", "ts", "tsx"] {
        for (consumer, blocking) in [
            (
                "export function readKey() { return recordLayout.key; }",
                false,
            ),
            ("export const key = recordLayout.options.stable;", false),
            ("export const level = recordLayout.options.level;", true),
            ("export const options = recordLayout.options;", true),
            ("export const all = recordLayout;", true),
            (
                "export const mixed = [recordLayout.key, recordLayout.options.level];",
                true,
            ),
        ] {
            let repo = object_repo(
                extension,
                "export const recordLayout = { key: 1, options: { level: 10, stable: true } };\n",
                "export const recordLayout = { key: 1, options: { level: 20, stable: true } };\n",
                consumer,
            );
            assert_object_impact(&repo, blocking);
        }
    }
    let repo = object_repo(
        "ts",
        "export const recordLayout = { key: 1, options: { level: 10 } };\n",
        "export const recordLayout = { key: 2, options: { level: 10 } };\n",
        "export function readKey() { return recordLayout.key; }",
    );
    assert_object_impact(&repo, true);
}

#[test]
fn impact_object_members_split_const_assertions_and_template_changes_in_review() {
    let before = "export const recordLayout = { endpoint: 'old', retries: 3, label: 'Example' };\n";
    let consumer = "export function title() { return recordLayout.label; }\nexport function attempts() { return recordLayout.retries; }\nexport function endpoint() { return recordLayout.endpoint; }";
    let asserted = before.replace("};", "} as const;");
    for (old, new, blocking, informational) in [
        (before, before.replace("3", "'3'"), vec![2], vec![0, 1, 3]),
        (
            asserted.as_str(),
            asserted.replace("3", "'3'"),
            vec![2],
            vec![0, 1, 3],
        ),
        (
            before,
            format!(
                "export const SERVER = 'new';\n{}",
                before.replace("'old'", "`${SERVER}/`")
            ),
            vec![3],
            vec![0, 1, 2],
        ),
        (
            before,
            before.replace("'old'", "`new/`"),
            vec![3],
            vec![0, 1, 2],
        ),
    ] {
        let repo = object_repo("ts", old, &new, consumer);
        for staged in [false, true] {
            let mut cmd = cargo_bin();
            cmd.args(["review", "--dir"])
                .arg(repo.root())
                .args(["--git", "--hook"]);
            if staged {
                repo.stage_all();
                repo.write("layout.ts", old);
                cmd.arg("--staged");
            }
            let output = cmd.output().unwrap();
            let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(output.status.code(), Some(1), "{json}");
            assert_eq!(json["api"]["mod"][0]["n"], "recordLayout", "{json}");
            let lines = |category: &str| {
                let mut lines: Vec<_> = json[category]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|change| change["refs"].as_array().unwrap())
                    .filter(|entry| entry["p"] == "consumer.ts")
                    .map(|entry| entry["ln"].as_u64().unwrap())
                    .collect();
                lines.sort_unstable();
                lines
            };
            assert_eq!(lines("impacts"), blocking, "{new} staged={staged}: {json}");
            assert_eq!(lines("impact_info"), informational, "{json}");
        }
    }
}

#[test]
fn impact_object_members_keep_whole_const_object_reads_blocking() {
    let before = "export const recordLayout = { url: 'old', keep: true } as const;\n";
    let after = before.replace("'old'", "`new/`");
    for consumer in [
        "consume(recordLayout);",
        "export const copy = { ...recordLayout };",
    ] {
        let repo = object_repo("ts", before, &after, consumer);
        assert_object_impact(&repo, true);
    }
}

#[test]
fn impact_object_members_keep_dynamic_and_non_read_consumers_blocking() {
    for consumer in [
        "export const key = recordLayout['key'];",
        "export const key = recordLayout[dynamicKey];",
        "export const key = recordLayout?.key;",
        "export const key = recordLayout.key();",
        "recordLayout.key = 2;",
        "[recordLayout.key] = [2];",
        "({ k: recordLayout.key } = source);",
        "for (recordLayout.key of values) {}",
        "recordLayout.key++;",
        "delete recordLayout.key;",
        "export const key = recordLayout.missing;",
        "export const key = recordLayout.key.deep;",
        "export type Layout = typeof recordLayout.key;",
        "export const { key } = recordLayout;",
    ] {
        let repo = object_repo(
            "ts",
            "export const recordLayout = { key: 1, options: { level: 10 } };\n",
            "export const recordLayout = { key: 1, options: { level: 20 } };\n",
            consumer,
        );
        assert_object_impact(&repo, true);
    }
}

#[test]
fn impact_object_members_keep_ambiguous_literals_and_header_changes_blocking() {
    for (before, after) in [
        (
            "{ key: 1, ...extra, level: 10 }",
            "{ key: 1, ...extra, level: 20 }",
        ),
        ("{ key: 1, [computed]: 10 }", "{ key: 1, [computed]: 20 }"),
        (
            "{ key: 1, key: 2, level: 10 }",
            "{ key: 1, key: 2, level: 20 }",
        ),
        (
            "{ key: 1, get level() { return 10; } }",
            "{ key: 1, get level() { return 20; } }",
        ),
        (
            "{ key: 1, level() { return 10; } }",
            "{ key: 1, level() { return 20; } }",
        ),
        ("{ key: 1, level: call(10) }", "{ key: 1, level: call(20) }"),
        (
            "{ key: 1, __proto__: {}, level: 10 }",
            "{ key: 1, __proto__: {}, level: 20 }",
        ),
        (
            "{ key: 1, nested: { ...extra }, level: 10 }",
            "{ key: 1, nested: { ...extra }, level: 20 }",
        ),
    ] {
        let repo = object_repo(
            "ts",
            &format!("export const recordLayout = {before};\n"),
            &format!("export const recordLayout = {after};\n"),
            "export const key = recordLayout.key;",
        );
        assert_object_impact(&repo, true);
    }
    for (before, after) in [
        (
            "export const recordLayout: { key: number, level: number } = { key: 1, level: 10 };\n",
            "export const recordLayout: { key: number, level: number | string } = { key: 1, level: 20 };\n",
        ),
        (
            "export const recordLayout = { key: 1, level: 10 };\n",
            "export let recordLayout = { key: 1, level: 20 };\n",
        ),
        (
            "export const recordLayout = { key: 1, level: 10 } as const;\n",
            "export const recordLayout = { key: 1, level: 20 };\n",
        ),
    ] {
        let repo = object_repo("ts", before, after, "export const key = recordLayout.key;");
        assert_object_impact(&repo, true);
    }
}

#[test]
fn impact_object_members_track_added_removed_and_replaced_paths() {
    for after in [
        "{ key: 1, options: { level: 10, added: true } }",
        "{ key: 1, options: {} }",
        "{ key: 1, options: 20 }",
    ] {
        for (consumer, blocking) in [
            ("export const key = recordLayout.key;", false),
            ("export const options = recordLayout.options;", true),
            ("export const level = recordLayout.options.level;", true),
        ] {
            let repo = object_repo(
                "ts",
                "export const recordLayout = { key: 1, options: { level: 10 } };\n",
                &format!("export const recordLayout = {after};\n"),
                consumer,
            );
            // 追加時だけ既存の葉は無変更。
            let blocking = blocking && !(after.contains("added") && consumer.contains(".level"));
            assert_object_impact(&repo, blocking);
        }
    }
}

#[test]
fn impact_object_members_use_index_source_and_validate_diff_reconstruction() {
    let before = "export const recordLayout = { key: 1, options: { level: 10 } };\n";
    let after = "export const recordLayout = { key: 1, options: { level: 20 } };\n";
    let repo = object_repo("ts", before, after, "export const key = recordLayout.key;");
    repo.git(["add", "layout.ts"]);
    repo.write(
        "layout.ts",
        "export const recordLayout = { key: 2, options: { level: 20 } };\n",
    );
    let staged = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--staged",
        ])
        .output()
        .unwrap();
    assert!(
        staged.status.success(),
        "{}",
        String::from_utf8_lossy(&staged.stderr)
    );
    assert_object_impact(&repo, true);

    repo.write("layout.ts", after);
    let diff = format!(
        "diff --git a/layout.ts b/layout.ts\n--- a/layout.ts\n+++ b/layout.ts\n@@ -1 +1 @@\n-{}+{}",
        before, after
    );
    let context = repo.run_json_with_stdin("context", &[], diff.as_bytes());
    assert_eq!(context["changes"].as_array().unwrap().len(), 1, "{context}");
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(|callers| callers.is_empty()),
        "{context}"
    );
    assert!(
        context["changes"][0]["informational_callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caller| caller["path"] == "consumer.ts" && caller["line"] == 1),
        "{context}"
    );
    let invalid_diff = diff.replace(
        after.trim(),
        "export const recordLayout = { key: 9, options: { level: 20 } };",
    );
    let context = repo.run_json_with_stdin("context", &[], invalid_diff.as_bytes());
    assert!(
        !context["changes"][0]["impacted_callers"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{context}"
    );
}

#[test]
fn impact_object_members_keep_same_named_sources_independent_and_output_deterministic() {
    let repo = object_repo(
        "ts",
        "export const recordLayout = { key: 1, options: { level: 10 } };\n",
        "export const recordLayout = { key: 1, options: { level: 20 } };\n",
        "export const key = recordLayout.key;",
    );
    repo.write(
        "other.ts",
        "export const recordLayout = { key: 1, options: { level: 10 } };\n",
    );
    repo.write(
        "other_consumer.ts",
        "import { recordLayout } from './other';\nexport const key = recordLayout.key;\n",
    );
    // 二つの変更元を同じ基点に置く。
    repo.write(
        "layout.ts",
        "export const recordLayout = { key: 1, options: { level: 10 } };\n",
    );
    repo.commit_all("second source");
    repo.write(
        "layout.ts",
        "export const recordLayout = { key: 1, options: { level: 20 } };\n",
    );
    repo.write(
        "other.ts",
        "export const recordLayout = { key: 2, options: { level: 10 } };\n",
    );
    let result = repo.run_json("context", &["--git"]);
    let changes = result["changes"].as_array().unwrap();
    let layout = changes
        .iter()
        .find(|change| change["path"] == "layout.ts")
        .unwrap();
    let other = changes
        .iter()
        .find(|change| change["path"] == "other.ts")
        .unwrap();
    assert!(
        layout["informational_callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caller| caller["path"] == "consumer.ts" && caller["line"] == 1),
        "{result}"
    );
    assert!(
        other["impacted_callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caller| caller["path"] == "other_consumer.ts" && caller["line"] == 1),
        "{result}"
    );
    // 直接importのない同名参照は、新しい判定で low から昇格しない。
    assert!(
        layout["low_confidence_callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caller| caller["path"] == "other_consumer.ts" && caller["line"] == 1),
        "{result}"
    );
    let mut outputs = Vec::new();
    for workers in ["1", "3", "1"] {
        let output = cargo_bin()
            .args(["context", "--dir", repo.root().to_str().unwrap(), "--git"])
            .env("ASTRO_SIGHT_IMPACT_WORKERS", workers)
            .output()
            .unwrap();
        assert!(output.status.success());
        outputs.push(output.stdout);
    }
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn impact_object_members_preserve_refs_and_dead_code_occurrences() {
    let repo = object_repo(
        "ts",
        "export const recordLayout = { key: 1, options: { level: 10 } };\n",
        "export const recordLayout = { key: 1, options: { level: 20 } };\n",
        "export const key = recordLayout.key;",
    );
    let references = repo.run_json(
        "refs",
        &["--name", "recordLayout", "--max-results", "unlimited"],
    );
    assert_eq!(
        references["refs"].as_array().unwrap().len(),
        3,
        "{references}"
    );
    let dead = repo.run_json("dead-code", &[]);
    assert!(!dead.to_string().contains("recordLayout"), "{dead}");
    assert_object_impact(&repo, false);
}

#[test]
fn impact_object_members_keep_source_mutation_and_escape_blocking() {
    for escape in [
        "recordLayout.key = recordLayout.options.level * 2;",
        "[recordLayout.key] = [2];",
        "({ k: recordLayout.key } = source);",
        "for (recordLayout.key of values) {}",
        "Object.assign(recordLayout, { key: recordLayout.options.level * 2 });",
        "Object.defineProperty(recordLayout, 'key', { value: recordLayout.options.level });",
        "const alias = recordLayout; alias.key = alias.options.level * 2;",
        "const boxed = { recordLayout }; boxed.recordLayout.key = 9;",
        "const options = recordLayout.options; options.level = 9;",
        "mutate(recordLayout.options);",
        "eval('recordLayout.key = recordLayout.options.level * 2');",
        "record\\u004cayout.key = record\\u004cayout.options.level * 2;",
    ] {
        for (old_escape, new_escape) in [(escape, escape), (escape, ""), ("", escape)] {
            let before = format!(
                "export const recordLayout = {{ key: 1, options: {{ level: 10 }} }};\n{old_escape}\n"
            );
            let after = format!(
                "export const recordLayout = {{ key: 1, options: {{ level: 20 }} }};\n{new_escape}\n"
            );
            let repo = object_repo(
                "ts",
                &before,
                &after,
                "export const key = recordLayout.key;",
            );
            assert_object_impact(&repo, true);
        }
    }
    let before = "export const recordLayout = { key: 1, options: { level: 10 } };\nexport function read() { return recordLayout.key; }\n";
    let after = "export const recordLayout = { key: 1, options: { level: 20 } };\nexport function read() { return recordLayout.key; }\n";
    let repo = object_repo("ts", before, after, "export const key = recordLayout.key;");
    assert_object_impact(&repo, false);
}

#[test]
fn impact_object_members_keep_type_annotations_and_jsdoc_blocking() {
    let repo = object_repo(
        "ts",
        "type Layout = { key: number, level: number };\nexport const recordLayout: Layout = { key: 1, level: 10 };\n",
        "type Layout = { key: string | number, level: number };\nexport const recordLayout: Layout = { key: 1, level: 20 };\n",
        "export const key: number = recordLayout.key;",
    );
    assert_object_impact(&repo, true);

    let repo = TestRepo::new();
    repo.write(
        "types.ts",
        "export type Layout = { key: number, level: number };\n",
    );
    repo.write("layout.ts", "import type { Layout } from './types';\nexport const recordLayout: Layout = { key: 1, level: 10 };\n");
    repo.write(
        "consumer.ts",
        "import { recordLayout } from './layout';\nexport const key: number = recordLayout.key;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "types.ts",
        "export type Layout = { key: string | number, level: number };\n",
    );
    repo.write("layout.ts", "import type { Layout } from './types';\nexport const recordLayout: Layout = { key: 1, level: 20 };\n");
    assert_object_impact(&repo, true);

    for tag in ["@type {Layout}", "@satisfies {Layout}"] {
        let before = format!(
            "// @ts-check\n/** @typedef {{{{ key: number, level: number }}}} Layout */\n/** {tag} */\nexport const recordLayout = {{ key: 1, level: 10 }};\n"
        );
        let after = format!(
            "// @ts-check\n/** @typedef {{{{ key: string | number, level: number }}}} Layout */\n/** {tag} */\nexport const recordLayout = {{ key: 1, level: 20 }};\n"
        );
        let repo = object_repo(
            "js",
            &before,
            &after,
            "export const key = recordLayout.key;",
        );
        assert_object_impact(&repo, true);
    }
}

#[test]
fn impact_object_members_keep_source_export_aliases_blocking() {
    for (export, blocking) in [
        (
            "export { recordLayout }; export { recordLayout as alias };",
            true,
        ),
        (
            "export { recordLayout }; export { recordLayout as default };",
            true,
        ),
        ("export { recordLayout };", false),
    ] {
        let before =
            format!("const recordLayout = {{ key: 1, options: {{ level: 10 }} }};\n{export}\n");
        let after =
            format!("const recordLayout = {{ key: 1, options: {{ level: 20 }} }};\n{export}\n");
        let repo = object_repo(
            "ts",
            &before,
            &after,
            "export const key = recordLayout.key;",
        );
        assert_object_impact(&repo, blocking);
    }
}

#[test]
fn impact_object_members_keep_aliased_module_consumers_blocking() {
    for (extension, writer, blocking_line, other_import_line, reference_count) in [
        (
            "ts",
            "import { recordLayout as rl } from './layout';\nrl.key = rl.options.level;\n",
            0,
            Some(0),
            5,
        ),
        (
            "ts",
            "export { recordLayout as rl } from './layout';\n",
            0,
            Some(0),
            5,
        ),
        (
            "ts",
            "import { recordLayout } from './layout';\nrecordLayout.key = recordLayout.options.level;\n",
            1,
            Some(0),
            7,
        ),
        (
            "js",
            "const { recordLayout: rl } = require('./layout');\nrl.key = rl.options.level;\n",
            0,
            Some(0),
            5,
        ),
        (
            "ts",
            "import * as NS from './layout';\nimport rl = NS.recordLayout;\nrl.key = rl.options.level;\n",
            1,
            Some(1),
            5,
        ),
        (
            "ts",
            "import * as NS from './layout';\nNS.recordLayout.key = NS.recordLayout.options.level;\n",
            1,
            None,
            6,
        ),
        (
            "ts",
            "const { recordLayout: rl } = await import('./layout');\nrl.key = rl.options.level;\n",
            0,
            None,
            5,
        ),
        (
            "ts",
            "const { recordLayout: rl } = await import(`./layout`);\nrl.key = rl.options.level;\n",
            0,
            None,
            5,
        ),
        (
            "ts",
            "const { recordLayout: rl } = await import(/* webpackChunkName: 'layout' */ './layout');\nrl.key = rl.options.level;\n",
            0,
            None,
            5,
        ),
        (
            "ts",
            "const m = await import('./layout');\nm.recordLayout.key = m.recordLayout.options.level;\n",
            1,
            None,
            6,
        ),
        (
            "ts",
            "import('./layout').then(({ recordLayout }) => { recordLayout.key = recordLayout.options.level; });\n",
            0,
            None,
            6,
        ),
    ] {
        let repo = TestRepo::new();
        let layout_path = format!("layout.{extension}");
        let consumer_path = format!("consumer.{extension}");
        let writer_path = format!("writer.{extension}");
        let other_path = format!("other.{extension}");
        repo.write(
            &layout_path,
            "export const recordLayout = { key: 1, options: { level: 10 } };\n",
        );
        repo.write(
            &consumer_path,
            "import { recordLayout } from './layout';\nexport const key = recordLayout.key;\n",
        );
        repo.write(&writer_path, writer);
        repo.write(
            &other_path,
            "export const recordLayout = { key: 1, level: value(10) };\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            &layout_path,
            "export const recordLayout = { key: 1, options: { level: 20 } };\n",
        );
        repo.write(
            &other_path,
            "export const recordLayout = { key: 1, level: value(20) };\n",
        );
        let mut outputs = Vec::new();
        for workers in ["1", "3", "1"] {
            let output = cargo_bin()
                .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
                .env("ASTRO_SIGHT_IMPACT_WORKERS", workers)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(1),
                "{writer}: {}",
                repo.run_json("context", &["--git"])
            );
            let context = cargo_bin()
                .args(["context", "--dir", repo.root().to_str().unwrap(), "--git"])
                .env("ASTRO_SIGHT_IMPACT_WORKERS", workers)
                .output()
                .unwrap();
            assert!(context.status.success());
            outputs.push(context.stdout);
        }
        assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
        let result: serde_json::Value = serde_json::from_slice(&outputs[0]).unwrap();
        let changes = result["changes"].as_array().unwrap();
        let layout = changes
            .iter()
            .find(|change| change["path"] == layout_path)
            .unwrap();
        let other = changes
            .iter()
            .find(|change| change["path"] == other_path)
            .unwrap();
        assert!(
            layout["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == writer_path && caller["line"] == blocking_line),
            "{result}"
        );
        assert!(
            layout["informational_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == consumer_path && caller["line"] == 1),
            "{result}"
        );
        if let Some(line) = other_import_line {
            assert!(
                other["informational_callers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|caller| caller["path"] == writer_path && caller["line"] == line),
                "{result}"
            );
        }
        let references = repo.run_json(
            "refs",
            &["--name", "recordLayout", "--max-results", "unlimited"],
        );
        assert_eq!(
            references["refs"].as_array().unwrap().len(),
            reference_count,
            "{references}"
        );
    }
}

#[test]
fn impact_object_members_dynamic_dependencies_preserve_other_source_low_routing() {
    for extension in ["js", "ts", "tsx"] {
        for (specifier, blocking) in [("'./layout'", true), ("`./layout`", true), ("path", false)] {
            let repo = TestRepo::new();
            let layout_path = format!("layout.{extension}");
            let other_path = format!("other.{extension}");
            let consumer_path = format!("consumer.{extension}");
            for path in [&layout_path, &other_path] {
                repo.write(
                    path,
                    "export const recordLayout = { key: 1, level: value(10) };\n",
                );
            }
            repo.write(
                &consumer_path,
                format!(
                    "const {{ recordLayout: rl }} = await import({specifier});\nconsume(rl);\n"
                ),
            );
            repo.init_git();
            repo.commit_all("initial");
            for path in [&layout_path, &other_path] {
                repo.write(
                    path,
                    "export const recordLayout = { key: 1, level: value(20) };\n",
                );
            }
            let output = cargo_bin()
                .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(if blocking { 1 } else { 0 }),
                "{extension}/{specifier}"
            );
            let result = repo.run_json("context", &["--git"]);
            let changes = result["changes"].as_array().unwrap();
            let layout = changes
                .iter()
                .find(|change| change["path"] == layout_path)
                .unwrap();
            let other = changes
                .iter()
                .find(|change| change["path"] == other_path)
                .unwrap();
            let bucket = if blocking {
                "impacted_callers"
            } else {
                "low_confidence_callers"
            };
            assert!(
                layout[bucket]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|caller| caller["path"] == consumer_path && caller["line"] == 0),
                "{result}"
            );
            assert!(
                other["low_confidence_callers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|caller| caller["path"] == consumer_path && caller["line"] == 0),
                "{result}"
            );
        }
    }
}

/// cross-file 検索に載らなかった (呼び出し元が 1 件も無い) ことを確かめる。
fn assert_no_object_callers(repo: &TestRepo) {
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("impact");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    for key in ["impacted_callers", "informational_callers"] {
        assert!(change.get(key).is_none(), "{key}: {context}");
    }
}

/// 複数行で書いたオブジェクトは名前が宣言ヘッダ行にしか無く、メンバーの削除・値の変更行には
/// 名前が出ない。フィルタ 5 (変更行に名前が無いシンボルは cross-file 検索しない) で落とさず、
/// 1 行で書いた同じ変更と同じ判定にする (旧実装は複数行の形だけ呼び出し元が 0 件で exit 0)。
#[test]
fn impact_object_members_multi_line_changes_match_one_line_form() {
    fn one_line(members: &[&str]) -> String {
        format!(
            "export const recordLayout = {{ {} }};\n",
            members.join(", ")
        )
    }
    fn multi_line(members: &[&str]) -> String {
        let body: String = members.iter().map(|m| format!("  {m},\n")).collect();
        format!("export const recordLayout = {{\n{body}}};\n")
    }
    // (変更前のメンバー, 変更後のメンバー, 呼び出し側, blocking か)
    let cases: [(&[&str], &[&str], &str, bool); 4] = [
        // メンバー削除: 消したメンバーの読み取りは blocking、残ったメンバーは情報提供。
        (
            &["key: 1", "extra: 2"],
            &["key: 1"],
            "export const extra = recordLayout.extra;",
            true,
        ),
        (
            &["key: 1", "extra: 2"],
            &["key: 1"],
            "export const key = recordLayout.key;",
            false,
        ),
        // 値の変更: 変わった値の読み取りは blocking、変わっていない値は情報提供。
        (
            &["key: 1", "options: { level: 10 }"],
            &["key: 1", "options: { level: 20 }"],
            "export const level = recordLayout.options.level;",
            true,
        ),
        (
            &["key: 1", "options: { level: 10 }"],
            &["key: 1", "options: { level: 20 }"],
            "export const key = recordLayout.key;",
            false,
        ),
    ];
    for extension in ["js", "ts", "tsx"] {
        for (before, after, consumer, blocking) in cases {
            for form in [one_line as fn(&[&str]) -> String, multi_line] {
                let repo = object_repo(extension, &form(before), &form(after), consumer);
                assert_object_impact(&repo, blocking);
            }
        }
        // 対照: メンバーを足しただけの変更は従来どおり cross-file 検索に載せない (フィルタ 3c)。
        let repo = object_repo(
            extension,
            &multi_line(&["key: 1"]),
            &multi_line(&["key: 1", "extra: 2"]),
            "export const key = recordLayout.key;",
        );
        assert_no_object_callers(&repo);
    }
    // `as const` で包んだ複数行のオブジェクトもメンバー削除は検索に載せる。
    // 対照として、追加だけの変更は従来どおり載せない。
    for extension in ["ts", "tsx"] {
        let wrapped = |members: &[&str]| multi_line(members).replace("\n};\n", "\n} as const;\n");
        let repo = object_repo(
            extension,
            &wrapped(&["key: 1", "extra: 2"]),
            &wrapped(&["key: 1"]),
            "export const extra = recordLayout.extra;",
        );
        assert_object_impact(&repo, true);
        let repo = object_repo(
            extension,
            &wrapped(&["key: 1"]),
            &wrapped(&["key: 1", "extra: 2"]),
            "export const key = recordLayout.key;",
        );
        assert_no_object_callers(&repo);
    }
}

/// JS/TS では `$` も識別子の文字。変更行の `$config` を `config` という語に分けて照合すると、
/// `$config` の変更行に名前が無いと判定して cross-file 検索から落としていた (exit 0)。
#[test]
fn impact_object_members_dollar_identifier_is_matched_on_changed_lines() {
    for name in ["$config", "config$", "config"] {
        let repo = TestRepo::new();
        repo.write(
            "config.ts",
            format!("export const {name} = {{ a: 1, b: 2 }};\n"),
        );
        repo.write(
            "use.ts",
            format!(
                "import {{ {name} }} from './config';\n\nexport function readB(): number {{\n  return {name}.b;\n}}\n"
            ),
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("config.ts", format!("export const {name} = {{ a: 1 }};\n"));
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .expect("impact");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{name}: {stderr}");
        assert!(stderr.contains("→ use.ts:4"), "{name}: {stderr}");
    }
}
