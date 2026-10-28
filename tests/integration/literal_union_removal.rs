//! 文字列 union の削除と、限定した値の流れの閉包を確認する。

use super::support::{TestRepo, cargo_bin};

const OLD_MODE: &str = r#"export type Mode = "all" | "legacy" | "recent";
export const MODE_LABELS: Record<Mode, string> = {
    all: "All",
    legacy: "Legacy",
    recent: "Recent",
};
export function matches(mode: Mode, age: number): boolean {
    switch (mode) {
        case "all": return true;
        case "legacy": return age > 30;
        case "recent": return age <= 7;
    }
}
"#;
const OLD_VIEW: &str = r#"import { type Mode, MODE_LABELS, matches } from "./mode";
const MODES: Mode[] = ["all", "legacy", "recent"];
export function visible(age: number): string[] {
    return MODES.filter((m) => matches(m, age)).map((m) => MODE_LABELS[m]);
}
"#;

fn union_fixture(extension: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let text = |source: &str| {
        if extension == "tsx" {
            format!("// 区分の表示\r\n{}", source.replace('\n', "\r\n"))
        } else {
            source.to_string()
        }
    };
    let mode = text(OLD_MODE);
    let view = text(OLD_VIEW);
    repo.write(format!("src/mode.{extension}"), &mode);
    repo.write(format!("src/view.{extension}"), &view);
    repo.commit_all("initial");
    repo.write(
        format!("src/mode.{extension}"),
        mode.replace(" | \"legacy\"", "")
            .replace("    legacy: \"Legacy\",\r\n", "")
            .replace("    legacy: \"Legacy\",\n", "")
            .replace("        case \"legacy\": return age > 30;\r\n", "")
            .replace("        case \"legacy\": return age > 30;\n", ""),
    );
    repo.write(
        format!("src/view.{extension}"),
        view.replace("\"all\", \"legacy\", \"recent\"", "\"all\", \"recent\""),
    );
    repo
}

fn union_hook(repo: &TestRepo, staged: bool, strict: bool) -> (i32, serde_json::Value) {
    let mut cmd = cargo_bin();
    cmd.args(["review", "--git", "--hook", "--format", "json", "--dir"])
        .arg(repo.root());
    if staged {
        cmd.arg("--staged");
    }
    if strict {
        cmd.arg("--strict-public-const-values");
    }
    let output = cmd.output().unwrap();
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stderr).unwrap(),
    )
}

fn union_category(code: i32, json: &serde_json::Value, expected: i32, closed: bool, case: &str) {
    assert_eq!(code, expected, "{case}: {json}");
    let bucket = if closed { "mod_closed" } else { "mod" };
    assert!(
        json["api"][bucket]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|e| e["n"] == "Mode")),
        "{case}: {json}"
    );
    if !closed {
        assert!(
            json["blocking_categories"]
                .as_array()
                .is_some_and(|categories| categories.iter().any(|category| category == "api.mod")),
            "{case}: {json}"
        );
        assert!(
            !json["api"]["mod_closed"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == "Mode")),
            "{case}: {json}"
        );
    }
}

fn union_rewrite(repo: &TestRepo, path: &str, old: &str, new: &str) {
    let source = std::fs::read_to_string(repo.root().join(path)).unwrap();
    assert!(source.contains(old), "missing fixture {old}");
    repo.write(path, source.replace(old, new));
}

#[test]
fn literal_union_removal_closes_record_switch_and_array() {
    for extension in ["ts", "tsx"] {
        let repo = union_fixture(extension);
        let refs = repo.run_json("refs", &["--name", "Mode"]);
        assert_eq!(refs["refs"].as_array().unwrap().len(), 5, "{refs}");
        assert_eq!(
            refs["refs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["kind"] == "def")
                .count(),
            1,
            "{refs}"
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 0, true, extension);
        assert!(
            hook["api"]["const_value"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["n"] == "MODE_LABELS"),
            "{hook}"
        );
        let normal = repo.run_json("review", &["--git"]);
        assert!(
            normal["api_changes"]["modified_closed_in_diff"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "Mode"),
            "{normal}"
        );
        let (code, strict) = union_hook(&repo, false, true);
        union_category(code, &strict, 1, true, "strict const remains blocking");
        repo.stage_all();
        repo.write(
            format!("src/mode.{extension}"),
            "export const worktreeOnly = 1;\n",
        );
        let (code, staged) = union_hook(&repo, true, false);
        union_category(code, &staged, 0, true, "index snapshot");
    }
}

#[test]
fn literal_union_removal_requires_each_removed_use_to_follow() {
    for (path, old, new) in [
        (
            "src/mode.ts",
            "    recent:",
            "    legacy: 'Legacy',\n    recent:",
        ),
        (
            "src/mode.ts",
            "        case \"recent\":",
            "        case 'legacy': return age > 30;\n        case \"recent\":",
        ),
        (
            "src/view.ts",
            "[\"all\", \"recent\"]",
            "[\"all\", \"legacy\", \"recent\"]",
        ),
    ] {
        let repo = union_fixture("ts");
        union_rewrite(&repo, path, old, new);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, new);
    }
}

#[test]
fn literal_union_removal_audits_exact_cooked_residue() {
    for (path, residue) in [
        ("src/extra.ts", "const dict = { legacy: 1 };"),
        ("src/extra.ts", "type X = 'legacy';"),
        ("src/extra.ts", "const text = `legacy`;"),
        ("src/extra.ts", "const text = 'leg\\u0061cy';"),
        (
            "src/extra.ts",
            "const text = '\\u006c\\u0065\\u0067\\u0061\\u0063\\u0079';",
        ),
        ("src/extra.ts", "const dict = { leg\\u0061cy: 1 };"),
        ("src/extra.ts", "const text = item.legacy;"),
        ("src/extra.js", "/** @type {'legacy'} */ let value;"),
        ("src/extra.vue", "<script>const x = 'legacy';</script>"),
        ("src/extra.json", "{\"legacy\": 1}"),
        ("src/extra.json", "{\"value\": \"leg\\u0061cy\"}"),
    ] {
        let repo = union_fixture("ts");
        repo.write(path, residue);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, residue);
    }
    let repo = union_fixture("ts");
    repo.write("src/extra.ts", "const base = 'legacy-peer'; const path = '\\n'; const unrelated = `${base}/${path}`; type OtherMode = 'other';\n");
    let (code, hook) = union_hook(&repo, false, false);
    union_category(
        code,
        &hook,
        0,
        true,
        "unrelated template and escaped newline",
    );
}

#[test]
fn literal_union_removal_keeps_queries_aliases_and_unknown_flows_blocking() {
    for extra in [
        "type X = typeof MODES[number];",
        "type X = keyof typeof MODE_LABELS;",
        "type X = Parameters<typeof matches>;",
        "const alias = MODES;",
        "const alias = MODE_LABELS;",
        "const alias = matches;",
        "MODES.push('all');",
        "MODE_LABELS['all'] = 'changed';",
        "matches(`leg${'acy'}` as never, 1);",
        "matches(externalMode, 1);",
        "matches('all' as Mode, 1);",
        "const value: Mode = 'all';",
        "const value = toExternal(MODES);",
        "const value = toExternal(MODE_LABELS);",
    ] {
        let repo = union_fixture("ts");
        let path = if extra.contains("MODE_LABELS") || extra.contains("matches") {
            "src/mode.ts"
        } else {
            "src/view.ts"
        };
        let mut text = std::fs::read_to_string(repo.root().join(path)).unwrap();
        text.push_str(extra);
        repo.write(path, text);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, extra);
    }
    for (path, old, new) in [
        (
            "src/view.ts",
            "MODES.filter((m) => matches(m, age))",
            "MODES.filter(matches)",
        ),
        (
            "src/view.ts",
            ".map((m) => MODE_LABELS[m])",
            ".map((m) => m)",
        ),
        (
            "src/view.ts",
            ".map((m) => MODE_LABELS[m])",
            ".map((m) => external(m))",
        ),
        ("src/mode.ts", "switch (mode)", "switch (mode = 'all')"),
        (
            "src/mode.ts",
            "switch (mode)",
            "var mode; mode = 'recent'; switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "({mode} = externalObject); switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "for ({mode} of externalObjects) {} switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "for (MODE_LABELS[mode] of values) {} switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "({x: MODE_LABELS[mode]} = externalObject); switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "if (mode === LIB.X) return false; switch (mode)",
        ),
    ] {
        let repo = union_fixture("ts");
        union_rewrite(&repo, path, old, new);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, new);
    }
    let repo = union_fixture("ts");
    repo.write(
        "src/other.ts",
        "export type Mode = 'other'; export const value: Mode = 'other';\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 0, true, "independent same-name type binding");
    let repo = union_fixture("ts");
    union_rewrite(&repo, "src/view.ts", "type Mode,", "type Mode as M,");
    union_rewrite(&repo, "src/view.ts", "MODES: Mode[]", "MODES: M[]");
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 1, false, "type import alias");
    for extra in [
        "export type { Mode } from './mode';",
        "import * as modes from './mode';",
        "declare const Record: unknown;",
        "Array.prototype.map = replacement;",
    ] {
        let repo = union_fixture("ts");
        repo.write("src/extra.ts", extra);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, extra);
    }
}

#[test]
fn literal_union_removal_uses_index_for_negative_snapshot() {
    let repo = union_fixture("ts");
    union_rewrite(
        &repo,
        "src/view.ts",
        "[\"all\", \"recent\"]",
        "[\"all\", \"legacy\", \"recent\"]",
    );
    repo.stage_all();
    union_rewrite(
        &repo,
        "src/view.ts",
        "[\"all\", \"legacy\", \"recent\"]",
        "[\"all\", \"recent\"]",
    );
    let (code, staged) = union_hook(&repo, true, false);
    union_category(code, &staged, 1, false, "index contains removed element");
    let (code, working) = union_hook(&repo, false, false);
    union_category(code, &working, 0, true, "working tree fixed");
}

#[test]
fn literal_union_removal_keeps_unsupported_alias_changes_blocking() {
    for declaration in [
        "export type Mode = \"recent\" | \"all\";",
        "export type Mode = \"all\" | \"new\";",
        "export type Mode<T> = \"all\" | \"recent\";",
        "type Other = \"all\"; export type Mode = Other | \"recent\";",
        "export type Mode = \"all\" | 1;",
        "export type Mode = \"all\" | \"r\\u0065cent\";",
        "export type Mode = \"all\" | \"all\" | \"recent\";",
        "export type Mode = \"all\" | \"recent\"; const Mode = 1;",
    ] {
        let repo = union_fixture("ts");
        union_rewrite(
            &repo,
            "src/mode.ts",
            "export type Mode = \"all\" | \"recent\";",
            declaration,
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, declaration);
    }
}

#[test]
fn literal_union_removal_checks_array_flow_and_ignored_residue_scope() {
    for (old, new) in [
        ("MODES: Mode[]", "MODES: readonly Mode[]"),
        ("MODES.filter((m) => matches(m, age)).map", "MODES.map"),
        (
            ".map((m) => MODE_LABELS[m])",
            ".filter((m) => m !== 'all').map((m) => MODE_LABELS[m])",
        ),
    ] {
        let repo = union_fixture("ts");
        union_rewrite(&repo, "src/view.ts", old, new);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 0, true, new);
    }
    let repo = union_fixture("ts");
    repo.write(".gitignore", "src/ignored.json\n");
    repo.write("src/ignored.json", "{\"legacy\": 1}");
    repo.write(
        "src/extra.ts",
        "const isArray = Array.isArray('unrelated');\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(
        code,
        &hook,
        0,
        true,
        "ignored JSON and ordinary builtin read",
    );
}

#[test]
fn literal_union_removal_keeps_dynamic_calls_and_resolution_barriers_blocking() {
    for (path, source) in [
        ("src/extra.ts", "eval('0');"),
        ("src/extra.ts", r"\u0065val('0');"),
        ("src/extra.ts", "new Function('return 0')();"),
        ("src/extra.ts", "const loaded = module.require('./mode');"),
        (
            "src/extra.ts",
            "const loaded = require.context('.', false, /mode/);",
        ),
        ("src/extra.ts", "const loaded = import.meta.glob('./*.ts');"),
        (
            "src/extra.ts",
            "import { createRequire as load } from 'node:module'; const loader = load(import.meta.url);",
        ),
        (
            "src/extra.ts",
            r"import { createRequ\u0069re as load } from 'node:module'; const loader = load(import.meta.url);",
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"rootDirs":["src","other"]}}"#,
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"root\u0044irs":["src","other"]}}"#,
        ),
        ("tsconfig.json", r#"{"extends":"./missing.json"}"#),
    ] {
        let repo = union_fixture("ts");
        repo.write(path, source);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, source);
    }
    for usage in [
        "MODES.filter?.((m) => matches(m, age)).map((m) => MODE_LABELS[m])",
        "MODES.filter((m) => matches(m, age)).map?.((m) => MODE_LABELS[m])",
        "MODES.filter((m) => matches(m, age)).map<string>((m) => MODE_LABELS[m])",
        "MODES.filter((m) => matches(m, age)).map((m) => MODE_LABELS[m], undefined)",
    ] {
        let repo = union_fixture("ts");
        union_rewrite(
            &repo,
            "src/view.ts",
            "MODES.filter((m) => matches(m, age)).map((m) => MODE_LABELS[m])",
            usage,
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, usage);
    }
    for transport in [
        "import * as hidden from '#mode';",
        "export * from '#mode';",
        "import hidden from 'unknown-package';",
    ] {
        let repo = union_fixture("ts");
        repo.write(
            "package.json",
            r##"{"name":"demo","imports":{"#mode":"./src/mode.ts"}}"##,
        );
        repo.write("src/extra.ts", transport);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, transport);
    }
    let repo = union_fixture("ts");
    repo.write("src/other.ts", "export const count = 1;\n");
    repo.write(
        "src/extra.ts",
        "import * as other from './other';\nimport { \"count\" as c } from './other';\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 0, true, "transport of proven different module");
}

fn compact_union_fixture(name: &str, old_extra: &str, new_extra: &str, argument: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    // 型参照を置いた全ての行を編集し、行ベース判定への復帰を検出する。
    let mode = format!(
        r#"export type {name} = "all" | "legacy" | "recent";
export const LABELS: Record<{name}, string> = {{ all: "All", legacy: "Legacy", recent: "Recent" }};
export function matches(mode: {name}, age: number): boolean {{ switch (mode) {{ case "all": return true; case "legacy": return age > 30; case "recent": return age <= 7; }} }}
"#
    );
    let view = format!(
        r#"import {{ type {name}, LABELS, matches }} from "./mode";
const MODES: {name}[] = ["all", "legacy", "recent"];
export function visible(age: number): string[] {{ return MODES.filter(m => matches(m, age)).map(m => LABELS[m]); }}
"#
    );
    repo.write("src/mode.tsx", format!("{mode}{old_extra}"));
    repo.write("src/view.tsx", &view);
    repo.commit_all("initial");
    repo.write(
        "src/mode.tsx",
        format!(
            "{}{new_extra}",
            mode.replace(" | \"legacy\"", "")
                .replace("legacy: \"Legacy\", ", "")
                .replace("case \"legacy\": return age > 30; ", "")
        ),
    );
    repo.write(
        "src/view.tsx",
        view.replace(", \"legacy\"", "")
            .replace("matches(m, age)", &format!("matches({argument}, age)")),
    );
    repo
}

#[test]
fn literal_union_removal_does_not_fall_back_for_unsupported_target_names() {
    for name in ["方式", r"Mo\u0064e"] {
        let repo = compact_union_fixture(name, "", "", "m");
        let (code, hook) = union_hook(&repo, false, false);
        assert_eq!(code, 1, "{name}: {hook}");
        assert!(
            hook["api"]["mod"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == name)),
            "{name}: {hook}"
        );
    }
}

#[test]
fn literal_union_removal_keeps_recognized_candidates_blocking_on_new_parse_limits() {
    let deep = format!(
        "\nconst deep = {}<span/>{};\n",
        "<div>".repeat(140),
        "</div>".repeat(140)
    );
    let broken = "\nconst broken = ;\n";
    for (old_extra, new_extra) in [
        ("", ""),
        ("", broken),
        ("", deep.as_str()),
        (broken, broken),
        (deep.as_str(), deep.as_str()),
    ] {
        let repo = compact_union_fixture(
            "Mode",
            old_extra,
            new_extra,
            "(globalThis as any).raw as never",
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(
            code,
            &hook,
            1,
            false,
            "unknown argument stays blocking despite new parse failure/limit",
        );
    }
}

#[test]
fn literal_union_removal_audits_embedded_and_import_require_transports() {
    for source in [
        r#"<script setup lang="ts">import { matches } from './mode'; const ok = matches(props.mode as never, 1);</script>"#,
        r#"<script setup lang="ts">import { type Mode } from './mode'; const props = defineProps<{mode: Mode}>();</script>"#,
        r#"<script setup lang="ts">import * as hidden from '#alias'; const exported = hidden;</script>"#,
    ] {
        let repo = union_fixture("ts");
        repo.write("src/Picker.vue", source);
        repo.write(
            "package.json",
            r##"{"imports":{"#alias":"./src/mode.ts"}}"##,
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, source);
    }
    for source in [
        "import m = require('./mode'); m['matches'](external as never, 1);",
        "export import m = require('./mode');",
        "import { \"matches\" as check } from './mode'; check((globalThis as any).raw as never, 1);",
        "import { \"MODE_LABELS\" as labels } from './mode'; const label = labels['all'];",
    ] {
        let repo = union_fixture("ts");
        repo.write("src/extra.ts", source);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, source);
    }
    let repo = union_fixture("ts");
    let mode = std::fs::read_to_string(repo.root().join("src/mode.ts")).unwrap();
    repo.write("src/mode.ts", format!("{mode}\nexport default matches;\n"));
    repo.write(
        "src/extra.ts",
        "import { default as check } from './mode'; const ok = check('all', 1);\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(
        code,
        &hook,
        1,
        false,
        "default export/import alias stays blocking",
    );
    let repo = union_fixture("ts");
    let output = cargo_bin()
        .current_dir(repo.root())
        .args([
            "review", "--dir", ".", "--git", "--hook", "--format", "json",
        ])
        .output()
        .unwrap();
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    union_category(
        output.status.code().unwrap(),
        &hook,
        0,
        true,
        "relative analysis root",
    );
}

#[test]
fn literal_union_removal_rejects_escaped_binding_writes_and_shadowing() {
    for prefix in [
        r"m\u006fde = (globalThis as any).raw;",
        r"{ let m\u006fde = (globalThis as any).raw; return MODE_LABELS[mode] !== ''; }",
        "function mode() {}",
    ] {
        let repo = union_fixture("ts");
        union_rewrite(
            &repo,
            "src/mode.ts",
            "switch (mode)",
            &format!("{prefix} switch (mode)"),
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, prefix);
    }
    let repo = union_fixture("ts");
    union_rewrite(
        &repo,
        "src/view.ts",
        "matches(m, age)",
        r"matches(m, (\u006d = (globalThis as any).raw, age))",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 1, false, "escaped callback parameter write");
}

#[test]
fn literal_union_removal_preserves_other_alias_classifications() {
    for (name, old_type, new_type, old_value, new_value) in [
        ("Flag", "true", "false", "true", "false"),
        ("Id", "string", "number", "\"a\"", "1"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let old = format!(
            "export type {name} = {old_type};\nexport function consume(value: {name}): string {{ return String(value) + 'old'; }}\n"
        );
        let new = format!(
            "export type {name} = {new_type};\nexport function consume(value: {name}): string {{ return String(value) + 'new'; }}\n"
        );
        let view = format!(
            "import {{ type {name}, consume }} from './types';\nconst input: {name} = {old_value};\nexport const output = consume(input);\n"
        );
        repo.write("types.ts", old);
        repo.write("view.ts", &view);
        repo.commit_all("initial");
        repo.write("types.ts", new);
        repo.write(
            "view.ts",
            view.replace(&format!("= {old_value};"), &format!("= {new_value};")),
        );
        let (code, hook) = union_hook(&repo, false, false);
        assert_eq!(code, 0, "{name}: {hook}");
        assert!(
            hook["api"]["mod_closed"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == name)),
            "{name}: {hook}"
        );
    }
}

#[test]
fn literal_union_removal_audits_ignored_resolution_inputs() {
    for case in 0..4 {
        let package_entry = case == 1;
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src/choice");
        let (target, other, spec, direct, metadata) = if package_entry {
            (
                "src/choice/target.ts",
                "src/choice/index.ts",
                "./choice",
                "./choice/target",
                "src/choice/package.json",
            )
        } else {
            (
                "src/mode.native.ts",
                "src/mode.ts",
                "./mode",
                "./mode.native",
                match case {
                    2 => "native/tsconfig.json",
                    3 => ".native/tsconfig.json",
                    _ => "src/tsconfig.json",
                },
            )
        };
        repo.write(target, OLD_MODE);
        repo.write(other, "export type Mode = string; export const MODE_LABELS: Record<Mode, string> = {}; export function matches(mode: Mode, age: number): boolean { return false; }\n");
        let view = OLD_VIEW.replace("./mode", spec);
        repo.write("src/view.ts", &view);
        repo.write(
            "src/safe.ts",
            format!("import {{ matches }} from '{direct}'; export const ok = matches('all', 1);\n"),
        );
        if case >= 2 {
            let dir = if case == 2 { "native" } else { ".native" };
            repo.create_dir_all(dir);
            repo.write(
                "tsconfig.json",
                format!(r#"{{"files":[],"references":[{{"path":"./{dir}"}}]}}"#),
            );
        }
        repo.write(".gitignore", format!("{metadata}\n"));
        repo.write(
            metadata,
            if package_entry {
                r#"{"types":"./target.ts"}"#
            } else {
                r#"{"compilerOptions":{"moduleSuffixes":[".native",""]}}"#
            },
        );
        repo.commit_all("initial");
        repo.write(
            target,
            OLD_MODE
                .replace(" | \"legacy\"", "")
                .replace("    legacy: \"Legacy\",\n", "")
                .replace("        case \"legacy\": return age > 30;\n", ""),
        );
        repo.write(
            "src/view.ts",
            view.replace(", \"legacy\"", "").replace(
                "matches(m, age)",
                "matches((globalThis as any).raw as never, age)",
            ),
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, metadata);
    }
}

#[test]
fn literal_union_removal_accepts_ignored_plain_ancestor_config() {
    let repo = union_fixture("ts");
    repo.write(".gitignore", "src/tsconfig.json\n");
    repo.write(
        "src/tsconfig.json",
        r#"{"compilerOptions":{"strict":true}}"#,
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 0, true, "ignored plain ancestor config");
}

#[test]
fn literal_union_removal_keeps_declaration_resolution_ambiguity_blocking() {
    for (declaration, other, spec) in [
        ("mode.d.ts", "mode/index.ts", "./mode"),
        ("mode.d.ts", "mode.js", "./mode"),
        ("mode.d.ts", "mode.js", "./mode.js"),
        ("mode.d.mts", "mode.mjs", "./mode.mjs"),
        ("mode.d.cts", "mode.cjs", "./mode.cjs"),
        ("mode.d.css.ts", "mode.css.ts", "./mode.css"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("package.json", r#"{"type":"module"}"#);
        repo.create_dir_all("mode");
        if declaration == "mode.d.css.ts" {
            repo.write(
                "tsconfig.json",
                r#"{"compilerOptions":{"allowArbitraryExtensions":true}}"#,
            );
        }
        repo.write(
            declaration,
            "export type Mode = 'all' | 'legacy' | 'recent';\n",
        );
        repo.write(
            other,
            if other.ends_with(".ts") {
                "export type Mode = string;\n"
            } else {
                "export class Mode {}\n"
            },
        );
        repo.write("safe.ts", format!("import type {{ Mode }} from './{declaration}';\nfunction check(mode: Mode): boolean {{ return mode === 'all'; }}\nexport const ok = check('all');\n"));
        repo.write("view.ts", format!("import {{ type Mode }} from '{spec}';\nexport const input: Mode = (globalThis as any).raw;\n"));
        repo.commit_all("initial");
        repo.write(declaration, "export type Mode = 'all' | 'recent';\n");
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, spec);
    }
}

#[test]
fn literal_union_removal_checks_implementation_before_explicit_declaration_import() {
    for (implementation, declaration) in [
        ("mode.ts", "mode.d.ts"),
        ("mode.mts", "mode.d.mts"),
        ("mode.cts", "mode.d.cts"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("package.json", r#"{"type":"module"}"#);
        let old = format!("{OLD_MODE}\nexport const ok = matches('all', 1);\n");
        repo.write(implementation, &old);
        repo.write(declaration, "export type Mode = string;\n");
        repo.write("view.ts", format!("import type {{ Mode }} from './{declaration}';\nexport const input: Mode = (globalThis as any).raw;\n"));
        repo.commit_all("initial");
        repo.write(
            implementation,
            old.replace(" | \"legacy\"", "")
                .replace("    legacy: \"Legacy\",\n", "")
                .replace("        case \"legacy\": return age > 30;\n", ""),
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, declaration);
    }
}

#[test]
fn literal_union_removal_checks_legacy_javascript_resolution_inputs() {
    for (module, resolution) in [("commonjs", "node10"), ("esnext", "classic")] {
        for directory_package in [false, true] {
            if directory_package && resolution == "classic" {
                continue;
            }
            let repo = TestRepo::new();
            repo.init_git();
            repo.create_dir_all("mode");
            let declaration = if directory_package {
                "mode/types.d.ts"
            } else {
                "mode.js.d.ts"
            };
            repo.write("tsconfig.json", format!(r#"{{"compilerOptions":{{"module":"{module}","moduleResolution":"{resolution}","strict":true}}}}"#));
            repo.write(
                declaration,
                "export type Mode = 'all' | 'legacy' | 'recent';\n",
            );
            repo.write("mode.js", "export class Mode {}\n");
            if directory_package {
                repo.write("mode/package.json", r#"{"types":"./types.d.ts"}"#);
            }
            repo.write("safe.ts", format!("import type {{ Mode }} from './{declaration}';\nfunction check(mode: Mode): boolean {{ return mode === 'all'; }}\nexport const ok = check('all');\n"));
            let specifier = if directory_package {
                "./mode"
            } else {
                "./mode.js"
            };
            repo.write("view.ts", format!("import type {{ Mode }} from '{specifier}';\nexport const input: Mode = (globalThis as any).raw;\n"));
            repo.commit_all("initial");
            repo.write(declaration, "export type Mode = 'all' | 'recent';\n");
            let (code, hook) = union_hook(&repo, false, false);
            union_category(
                code,
                &hook,
                1,
                false,
                &format!("{resolution}/{declaration}"),
            );
        }
    }
}
