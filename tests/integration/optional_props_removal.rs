//! inline object 型の props から省略可能プロパティを消した変更を、リポジトリ内の JSX 要素と
//! 直接呼び出しの証拠で互換に分類する (Issue #208)。

use super::support::{TestRepo, cargo_bin};

const PANEL_BEFORE: &str = "export function Panel({\n\tlabel,\n\tlegacy,\n\tonDone,\n}: {\n\tlabel: string;\n\tlegacy?: { id: string };\n\tonDone: () => void;\n}) {\n\treturn (\n\t\t<button type=\"button\" onClick={() => onDone()}>\n\t\t\t{label}\n\t\t\t{legacy?.id}\n\t\t</button>\n\t);\n}\n";
const PANEL_AFTER: &str = "export function Panel({\n\tlabel,\n\tonDone,\n}: {\n\tlabel: string;\n\tonDone: () => void;\n}) {\n\treturn (\n\t\t<button type=\"button\" onClick={() => onDone()}>\n\t\t\t{label}\n\t\t</button>\n\t);\n}\n";
const SCREEN: &str = "import { Panel } from \"./panel\";\n\nexport function Screen() {\n\treturn <Panel label=\"Open\" onDone={() => {}} />;\n}\n";
const CALL: &str = "<Panel label=\"Open\" onDone={() => {}} />";

/// `src/panel.tsx` の旧版と `files` をコミットしてから、`src/panel.tsx` を新版に書き換える。
fn props_repo(before: &str, after: &str, files: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/panel.tsx", before);
    for (path, source) in files {
        if let Some(parent) = std::path::Path::new(path).parent() {
            repo.create_dir_all(parent);
        }
        repo.write(path, source);
    }
    repo.commit_all("initial");
    repo.write("src/panel.tsx", after);
    repo
}

fn hook_with(repo: &TestRepo, extra: &[&str]) -> (std::process::Output, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook"])
        .args(extra)
        .arg("--dir")
        .arg(repo.root())
        .output()
        .unwrap();
    let json = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "invalid hook output: {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, json)
}

fn hook(repo: &TestRepo) -> (std::process::Output, serde_json::Value) {
    hook_with(repo, &[])
}

fn has_ref(groups: &serde_json::Value, path: &str, line: u64) -> bool {
    groups.as_array().into_iter().flatten().any(|group| {
        group["refs"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|reference| reference["p"] == path && reference["ln"] == line)
    })
}

fn lists(items: &serde_json::Value, name: &str) -> bool {
    items
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["n"] == name)
}

/// 互換に分類され、終了コードが 0 で、呼び出し箇所が impact_info に残る。
fn assert_compatible(repo: &TestRepo, name: &str, context: &str) -> serde_json::Value {
    let (output, json) = hook(repo);
    assert!(output.status.success(), "{context}: {json}");
    assert!(
        json["api"]["mod_compat"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| item["n"] == name && item["reason"] == "optional_object_props_removed"),
        "{context}: {json}"
    );
    assert!(!lists(&json["api"]["mod"], name), "{context}: {json}");
    assert!(json["impacts"].is_null(), "{context}: {json}");
    json
}

/// 従来どおり api.mod と impacts で止まり、互換に分類されない。
fn assert_blocking(repo: &TestRepo, name: &str, context: &str) -> serde_json::Value {
    let (output, json) = hook(repo);
    assert_eq!(output.status.code(), Some(1), "{context}: {json}");
    assert!(lists(&json["api"]["mod"], name), "{context}: {json}");
    assert!(
        !lists(&json["api"]["mod_compat"], name),
        "{context}: {json}"
    );
    json
}

fn screen_with(module_items: &str, call: &str) -> String {
    format!(
        "import {{ Panel }} from \"./panel\";\n{module_items}\nexport function Screen() {{\n\treturn {call};\n}}\n"
    )
}

#[test]
fn optional_props_removal_changed_callers_require_removed_key_proof() {
    for (extension, name, before, after, old_call, cases) in [
        (
            "tsx",
            "Panel",
            "export function Panel({ label, legacy, children }: { label: string; legacy?: { id: string }; children?: string }) { return <>{label}</>; }\n",
            "export function Panel({ label }: { label: string }) { return <>{label}</>; }\n",
            "<Panel label=\"old\" />",
            vec![
                ("<Panel label=\"new\" legacy={{ id: \"a\" }} />", false),
                ("<Panel label=\"new\" {...rest} />", false),
                ("<Panel label=\"new\">child</Panel>", false),
                ("<Panel label=\"new\" />", true),
            ],
        ),
        (
            "ts",
            "configure",
            "export function configure({ label, legacy }: { label: string; legacy?: boolean }): string { return label; }\n",
            "export function configure({ label }: { label: string }): string { return label; }\n",
            "configure({ label: \"old\" })",
            vec![
                ("configure({ label: \"new\", legacy: true })", false),
                ("configure({ label: \"new\", \"legacy\": true })", false),
                ("configure({ label: \"new\", [key]: true })", false),
                ("configure({ label: \"new\", ...rest })", false),
                ("configure(rest)", false),
                ("configure({ label: \"new\" })", true),
                ("setup({ label: \"new\", legacy: true })", false),
                ("setup({ label: \"new\" })", true),
            ],
        ),
    ] {
        for (call, closed) in cases {
            let repo = TestRepo::new();
            repo.init_git();
            repo.create_dir_all("src");
            let target = format!("src/target.{extension}");
            let caller = format!("src/caller.{extension}");
            let import = if call.starts_with("setup(") {
                format!("{name} as setup")
            } else {
                name.to_owned()
            };
            let old_call = if call.starts_with("setup(") {
                old_call.replace(name, "setup")
            } else {
                old_call.to_owned()
            };
            let source = |call: &str| {
                format!(
                    "import {{ {import} }} from './target';\nconst rest = {{ label: 'rest' }}; const key = 'legacy';\nfunction Screen() {{ return {call}; }}\nScreen();\n"
                )
            };
            repo.write(&target, before);
            repo.write(&caller, source(&old_call));
            repo.write("src/jsx.d.ts", "declare namespace JSX { interface Element {} interface IntrinsicElements {} interface ElementChildrenAttribute { children: {}; } }\n");
            repo.commit_all("initial");
            repo.write(&target, after);
            repo.write(&caller, source(call));
            for staged in [false, true] {
                if staged {
                    repo.git(["add", "."]);
                    // index の不正な呼び出しを、worktree の正しい呼び出しで隠せない。
                    repo.write(&caller, source(&old_call));
                }
                let (output, json) = hook_with(&repo, if staged { &["--staged"] } else { &[] });
                assert_eq!(
                    output.status.code(),
                    Some(i32::from(!closed)),
                    "{call}, staged={staged}: {json}"
                );
                assert_eq!(
                    lists(&json["api"]["mod_closed"], name),
                    closed,
                    "{call}: {json}"
                );
                assert_eq!(lists(&json["api"]["mod"], name), !closed, "{call}: {json}");
                assert_eq!(
                    has_ref(&json["impacts"], &caller, 2),
                    !closed,
                    "{call}: {json}"
                );
            }
        }
    }
}

#[test]
fn optional_props_removal_changed_type_references_require_member_proof() {
    for declaration in [
        "export interface Props { label: string; legacy?: boolean; }\n",
        "export type Props = { label: string; legacy?: boolean; };\n",
    ] {
        for (value, closed) in [
            ("{ label: 'new', legacy: true }", false),
            ("{ label: 'new' }", true),
        ] {
            let repo = TestRepo::new();
            repo.init_git();
            repo.write("target.ts", declaration);
            repo.write("caller.ts", "import { type Props } from './target';\nconst value: Props = { label: 'old' };\nconsole.log(value.label);\n");
            repo.commit_all("initial");
            repo.write("target.ts", declaration.replace(" legacy?: boolean;", ""));
            repo.write("caller.ts", format!("import {{ type Props }} from './target';\nconst value: Props = {value};\nconsole.log(value.label);\n"));
            let (output, json) = hook(&repo);
            assert_eq!(
                output.status.code(),
                Some(i32::from(!closed)),
                "{value}: {json}"
            );
            assert_eq!(
                lists(&json["api"]["mod_closed"], "Props"),
                closed,
                "{value}: {json}"
            );
            assert_eq!(
                has_ref(&json["impacts"], "caller.ts", 1),
                !closed,
                "{value}: {json}"
            );
        }
    }
}

#[test]
fn optional_props_removal_issue_repro_is_mod_compat() {
    let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &[("src/screen.tsx", SCREEN)]);
    let json = assert_compatible(&repo, "Panel", "issue repro");
    assert_eq!(json["api"]["mod_compat"][0]["f"], "src/panel.tsx", "{json}");
    assert!(
        json["blocking_categories"].as_array().unwrap().is_empty(),
        "{json}"
    );
    // 証拠にした JSX の呼び出し (3 行目) は確認箇所として残す。
    assert!(has_ref(&json["impact_info"], "src/screen.tsx", 3), "{json}");
    let api = repo.run_json("review", &["--git"]);
    assert_eq!(
        api["api_changes"]["compatible_modified"][0]["reason"], "optional_object_props_removed",
        "{api}"
    );
    let impact = cargo_bin()
        .args(["impact", "--dir"])
        .arg(repo.root())
        .arg("--git")
        .output()
        .unwrap();
    assert!(impact.status.success(), "{impact:?}");
}

#[test]
fn optional_props_removal_supplied_or_spread_callers_stay_blocking() {
    let props = "const props = { label: \"Open\", onDone: () => {} };";
    for call in [
        "<Panel label=\"Open\" legacy={{ id: \"a\" }} onDone={() => {}} />",
        "<Panel {...props} />",
        "<Panel label=\"Open\" legacy={undefined} onDone={() => {}} />",
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[("src/screen.tsx", &screen_with(props, call))],
        );
        let json = assert_blocking(&repo, "Panel", call);
        assert!(
            has_ref(&json["impacts"], "src/screen.tsx", 3),
            "{call}: {json}"
        );
    }
    // 対照: 属性も spread も渡さない同じ構成は互換。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[("src/screen.tsx", &screen_with(props, CALL))],
    );
    assert_compatible(&repo, "Panel", "control");
}

#[test]
fn optional_props_removal_rejects_unproven_uses() {
    for hazard in [
        "const C = Panel;",
        "type P = Parameters<typeof Panel>[0];",
        "const props = { label: \"x\", onDone: () => {} }; Panel(props);",
        "Panel({ label: \"x\", onDone: () => {} });",
        "function h(component: unknown, props: unknown) { return [component, props]; } h(Panel, {});",
        "Panel.displayName = \"P\";",
        "export { Panel };",
        "const view = <Panel<string> label=\"x\" onDone={() => {}} />;",
        "function Shadow({ Panel }: { Panel: () => null }) { return <Panel />; }",
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[("src/screen.tsx", &screen_with(hazard, CALL))],
        );
        assert_blocking(&repo, "Panel", hazard);
    }
}

#[test]
fn optional_props_removal_rejects_hidden_module_transport() {
    for (path, source) in [
        (
            "src/alias.tsx",
            "import { Panel as P } from \"./panel\";\nexport const view = <P label=\"x\" onDone={() => {}} />;\n",
        ),
        (
            "src/namespace.tsx",
            "import * as ns from \"./panel\";\nexport const view = <ns.Panel label=\"x\" onDone={() => {}} />;\n",
        ),
        ("src/index.ts", "export { Panel } from \"./panel\";\n"),
        ("src/index.ts", "export * from \"./panel\";\n"),
        (
            "src/types.ts",
            "import type { Panel } from \"./panel\";\nexport type T = typeof Panel;\n",
        ),
        (
            "src/lazy.ts",
            "export const load = () => import(\"./panel\");\n",
        ),
        (
            "src/legacy.ts",
            "export const panel = require(\"./panel\");\n",
        ),
        ("src/run.ts", "eval(\"Panel\");\n"),
        ("src/component.vue", "<template><div /></template>\n"),
        ("tools/script.py", "print(1)\n"),
        ("src/broken.ts", "const broken = ;\n"),
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[("src/screen.tsx", SCREEN), (path, source)],
        );
        assert_blocking(&repo, "Panel", source);
    }
}

#[test]
fn optional_props_removal_rejects_contract_changes() {
    for (before, after, screen) in [
        // 残りの型の変更・必須の削除・削除と追加の同時
        (
            PANEL_BEFORE.to_owned(),
            PANEL_AFTER.replace("label: string;", "label: number;"),
            SCREEN.to_owned(),
        ),
        (
            PANEL_BEFORE.replace("legacy?:", "legacy:"),
            PANEL_AFTER.to_owned(),
            SCREEN.to_owned(),
        ),
        (
            PANEL_BEFORE.to_owned(),
            PANEL_AFTER.replace("\tonDone: () => void;\n}", "\tonDone: () => void;\n\textra?: string;\n}"),
            SCREEN.to_owned(),
        ),
        // 戻り値・async・default export
        (
            PANEL_BEFORE.to_owned(),
            PANEL_AFTER.replace("}) {", "}): unknown {"),
            SCREEN.to_owned(),
        ),
        (
            PANEL_BEFORE.to_owned(),
            PANEL_AFTER.replace("export function", "export async function"),
            SCREEN.to_owned(),
        ),
        (
            PANEL_BEFORE.replace("export function", "export default function"),
            PANEL_AFTER.replace("export function", "export default function"),
            SCREEN.replace("import { Panel }", "import Panel"),
        ),
        // 注釈の無い戻り値を分類できない (値を返す)
        (
            PANEL_BEFORE.to_owned(),
            "export function Panel({\n\tlabel,\n\tonDone,\n}: {\n\tlabel: string;\n\tonDone: () => void;\n}) {\n\treturn label;\n}\n".to_owned(),
            SCREEN.to_owned(),
        ),
        // 残った props が参照する同じファイルの型別名の変更
        (
            format!("type Done = () => void;\n{}", PANEL_BEFORE.replace("onDone: () => void;", "onDone: Done;")),
            // `onDone={() => {}}` は `() => string` を満たさなくなる。
            format!(
                "type Done = () => string;\n{}",
                PANEL_AFTER.replace("onDone: () => void;", "onDone: Done;")
            ),
            SCREEN.to_owned(),
        ),
        // children を消すと、子要素を書いた要素が渡す
        (
            PANEL_BEFORE.replace("legacy?: { id: string };", "children?: string;").replace("\tlegacy,\n", "\tchildren,\n").replace("{legacy?.id}", "{children}"),
            PANEL_AFTER.to_owned(),
            SCREEN.replace(
                "<Panel label=\"Open\" onDone={() => {}} />",
                "<Panel label=\"Open\" onDone={() => {}}>text</Panel>",
            ),
        ),
    ] {
        let repo = props_repo(&before, &after, &[("src/screen.tsx", &screen)]);
        assert_blocking(&repo, "Panel", &after);
    }
    // 対照: 同じファイルの型別名が変わらなければ互換。
    let repo = props_repo(
        &format!(
            "type Done = () => void;\n{}",
            PANEL_BEFORE.replace("onDone: () => void;", "onDone: Done;")
        ),
        &format!(
            "type Done = () => void;\n{}",
            PANEL_AFTER.replace("onDone: () => void;", "onDone: Done;")
        ),
        &[("src/screen.tsx", SCREEN)],
    );
    assert_compatible(&repo, "Panel", "unchanged alias");
}

#[test]
fn optional_props_removal_rejects_replaced_jsx_environment() {
    for (path, source) in [
        (
            "tsconfig.json",
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"jsxImportSource\": \"preact\" } }\n",
        ),
        (
            "src/jsx.d.ts",
            "declare global {\n\tnamespace JSX {\n\t\tinterface IntrinsicAttributes {\n\t\t\tlegacy?: unknown;\n\t\t}\n\t}\n}\nexport {};\n",
        ),
        (
            "src/pragma.tsx",
            "/** @jsxImportSource preact */\nexport const view = <div />;\n",
        ),
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[("src/screen.tsx", SCREEN), (path, source)],
        );
        assert_blocking(&repo, "Panel", source);
    }
    // 対照: React の標準の設定は互換のまま。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (
                "tsconfig.json",
                "{\n\t// JSONC\n\t\"compilerOptions\": { \"jsx\": \"react-jsx\", \"strict\": true, },\n}\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "react tsconfig");
}

#[test]
fn optional_props_removal_direct_calls_require_explicit_return() {
    let before = "export function make(options: { a: number; legacy?: string }): number { return options.a; }\n";
    let after = "export function make(options: { a: number }): number { return options.a; }\n";
    let repo_for = |before: &str, after: &str, usage: &str| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("make.ts", before);
        repo.write(
            "use.ts",
            format!("import {{ make }} from './make';\nexport const value = {usage};\n"),
        );
        repo.commit_all("initial");
        repo.write("make.ts", after);
        repo
    };
    let repo = repo_for(before, after, "make({ a: 1 })");
    let json = assert_compatible(&repo, "make", "annotated call");
    assert!(has_ref(&json["impact_info"], "use.ts", 1), "{json}");
    for usage in [
        "make({ a: 1, legacy: 'x' })",
        "make({ a: 1, 'legacy': 'x' })",
        "make({ ...{ a: 1 } })",
        "make({ a: 1, legacy() { return 'x'; } })",
    ] {
        assert_blocking(&repo_for(before, after, usage), "make", usage);
    }
    // 注釈の無い戻り値は直接呼び出しの証拠に使わない。
    assert_blocking(
        &repo_for(
            &before.replace("): number {", ") {"),
            &after.replace("): number {", ") {"),
            "make({ a: 1 })",
        ),
        "make",
        "inferred return",
    );
}

#[test]
fn optional_props_removal_checks_uses_in_the_declaring_file() {
    let demo = |call: &str| format!("\nexport function Demo() {{\n\treturn {call};\n}}\n");
    let repo = props_repo(
        &format!("{PANEL_BEFORE}{}", demo(CALL)),
        &format!("{PANEL_AFTER}{}", demo(CALL)),
        &[("src/screen.tsx", SCREEN)],
    );
    assert_compatible(&repo, "Panel", "same-file use without the key");
    let supplied = "<Panel label=\"x\" legacy={{ id: \"a\" }} onDone={() => {}} />";
    let repo = props_repo(
        &format!("{PANEL_BEFORE}{}", demo(supplied)),
        &format!("{PANEL_AFTER}{}", demo(supplied)),
        &[("src/screen.tsx", SCREEN)],
    );
    assert_blocking(&repo, "Panel", "same-file use with the key");
}

#[test]
fn optional_props_removal_tolerates_unrelated_named_imports() {
    let screen = SCREEN.replace(
        "import { Panel } from \"./panel\";\n",
        "import { Panel } from \"./panel\";\nimport { useState } from \"react\";\n",
    ) + "export const state = useState;\n";
    let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &[("src/screen.tsx", &screen)]);
    assert_compatible(&repo, "Panel", "unrelated named import");
    // 対照: 対象と同じ名前を運び得る import は、従来どおり在庫を閉じられない。
    for import in [
        "import * as React from \"react\";\n",
        "import React, { useState } from \"react\";\n",
        "import { Panel as Other } from \"ui\";\n",
        "import { default as Thing } from \"ui\";\n",
    ] {
        let screen = SCREEN.replace(
            "import { Panel } from \"./panel\";\n",
            &format!("import {{ Panel }} from \"./panel\";\n{import}"),
        );
        let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &[("src/screen.tsx", &screen)]);
        assert_blocking(&repo, "Panel", import);
    }
}

#[test]
fn optional_props_removal_bare_import_types_need_an_isolated_diff() {
    let with_icon = |source: &str| {
        format!(
            "import type {{ ReactNode }} from \"react\";\n{}",
            source.replace(
                "\tlabel: string;\n",
                "\tlabel: string;\n\ticon?: ReactNode;\n"
            )
        )
    };
    let repo = props_repo(
        &with_icon(PANEL_BEFORE),
        &with_icon(PANEL_AFTER),
        &[
            ("src/screen.tsx", SCREEN),
            ("src/notes.ts", "const note = 1;\n"),
        ],
    );
    assert_compatible(&repo, "Panel", "only the component changed");
    // 解決先の分からない型は、ほかのファイルも変えた diff では証明しない。
    repo.write("src/notes.ts", "const note = 2;\n");
    assert_blocking(&repo, "Panel", "another file changed");
    // 対照: 外部の型を参照しない props なら、ほかのファイルを変えても互換。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            ("src/notes.ts", "const note = 1;\n"),
        ],
    );
    repo.write("src/notes.ts", "const note = 2;\n");
    assert_compatible(&repo, "Panel", "no external types");
}

#[test]
fn optional_props_removal_arrow_component_stays_blocking() {
    let arrow = |source: &str| {
        source
            .replacen("export function Panel(", "export const Panel = (", 1)
            .replacen("}) {\n\treturn (", "}) => {\n\treturn (", 1)
    };
    let repo = props_repo(
        &arrow(PANEL_BEFORE),
        &arrow(PANEL_AFTER),
        &[("src/screen.tsx", SCREEN)],
    );
    assert_blocking(&repo, "Panel", "arrow component");
}

#[test]
fn optional_props_removal_preserves_other_blocking_origins() {
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (
                "src/other.ts",
                "export function other(): number { return 1; }\n",
            ),
            (
                "src/use.ts",
                "import { other } from \"./other\";\nexport const value = other();\n",
            ),
        ],
    );
    repo.write(
        "src/other.ts",
        "export function other(value: number): number { return value; }\n",
    );
    let (output, json) = hook(&repo);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(lists(&json["api"]["mod_compat"], "Panel"), "{json}");
    assert!(lists(&json["api"]["mod"], "other"), "{json}");
    let groups = json["impacts"].as_array().unwrap();
    assert!(
        groups.iter().any(|group| group["src"] == "src/other.ts"),
        "{json}"
    );
    assert!(
        groups.iter().all(|group| group["src"] != "src/panel.tsx"),
        "{json}"
    );
}

#[test]
fn optional_props_removal_staged_review_reads_the_index() {
    let supplied = SCREEN.replace(
        "<Panel label=\"Open\" onDone={() => {}} />",
        "<Panel label=\"Open\" legacy={{ id: \"a\" }} onDone={() => {}} />",
    );
    // index は呼び出しを変えず、作業ツリーだけが削除したキーを渡す。
    let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &[("src/screen.tsx", SCREEN)]);
    repo.git(["add", "src/panel.tsx"]);
    repo.write("src/screen.tsx", &supplied);
    let (output, json) = hook_with(&repo, &["--staged"]);
    assert!(output.status.success(), "{json}");
    assert_eq!(
        json["api"]["mod_compat"][0]["reason"], "optional_object_props_removed",
        "{json}"
    );
    // index にだけ JSX の import 元を差し替える tsconfig がある (作業ツリーでは消した)。
    let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &[("src/screen.tsx", SCREEN)]);
    repo.write(
        "tsconfig.json",
        "{ \"compilerOptions\": { \"jsxImportSource\": \"preact\" } }\n",
    );
    repo.stage_all();
    repo.remove_file("tsconfig.json");
    let (output, json) = hook_with(&repo, &["--staged"]);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(lists(&json["api"]["mod"], "Panel"), "{json}");
    // 対照: 作業ツリーの内容 (tsconfig なし) で判定すれば互換になる。
    let (output, json) = hook(&repo);
    assert!(output.status.success(), "{json}");
}

#[test]
fn optional_props_addition_keeps_its_reason() {
    let after = PANEL_BEFORE
        .replace("\tonDone,\n}", "\tonDone,\n\textra,\n}")
        .replace(
            "\tonDone: () => void;\n}",
            "\tonDone: () => void;\n\textra?: string;\n}",
        )
        .replace("{legacy?.id}\n", "{legacy?.id}\n\t\t\t{extra}\n");
    let repo = props_repo(PANEL_BEFORE, &after, &[("src/screen.tsx", SCREEN)]);
    let (output, json) = hook(&repo);
    assert!(output.status.success(), "{json}");
    assert_eq!(
        json["api"]["mod_compat"][0]["reason"], "optional_object_props",
        "{json}"
    );
}

#[test]
fn optional_props_removal_relative_import_types_are_not_proven() {
    // 解決先 (types.ts) が変わらなくても、その先の leaf.ts の変更で Options の型が変わる。
    let repo_for = |leaf_after: Option<&str>| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write(
            "src/leaf.ts",
            "type Scalar = string;\nexport interface Options { value: Scalar }\n",
        );
        repo.write("src/types.ts", "export type { Options } from \"./leaf\";\n");
        repo.write(
            "src/panel.tsx",
            "import type { Options } from \"./types\";\nexport function Panel({ options, legacy }: { options: Options; legacy?: string }) {\n\treturn null;\n}\n",
        );
        repo.write(
            "src/screen.tsx",
            "import { Panel } from \"./panel\";\nexport function Screen() {\n\treturn <Panel options={{ value: \"a\" }} />;\n}\n",
        );
        repo.commit_all("initial");
        repo.write(
            "src/panel.tsx",
            "import type { Options } from \"./types\";\nexport function Panel({ options }: { options: Options }) {\n\treturn null;\n}\n",
        );
        if let Some(leaf) = leaf_after {
            repo.write("src/leaf.ts", leaf);
        }
        repo
    };
    assert_blocking(
        &repo_for(Some(
            "type Scalar = number;\nexport interface Options { value: Scalar }\n",
        )),
        "Panel",
        "re-exported type changed",
    );
    // 相対 import の型は推移的にたどらないので、ほかのファイルを変えていなくても証明しない。
    assert_blocking(&repo_for(None), "Panel", "relative import type");
    // 対照: 相対 import の型を使わない形は互換 (registry の依存の型は受け入れる)。
    let with_icon = |source: &str| {
        format!(
            "import type {{ ReactNode }} from \"react\";\n{}",
            source.replace(
                "\tlabel: string;\n",
                "\tlabel: string;\n\ticon?: ReactNode;\n"
            )
        )
    };
    let repo = props_repo(
        &with_icon(PANEL_BEFORE),
        &with_icon(PANEL_AFTER),
        &[("src/screen.tsx", SCREEN)],
    );
    assert_compatible(&repo, "Panel", "registry type");
}

#[test]
fn optional_props_removal_rejects_types_returning_through_the_declaring_file() {
    // types.ts は宣言のファイルの別の export (scalar) の戻り値から Options を作る。宣言のファイル
    // だけを変えた diff でも、Options は string から number に変わる。
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write(
        "src/types.ts",
        "import { scalar } from \"./panel\";\nexport type Options = ReturnType<typeof scalar>;\n",
    );
    let panel = |value: &str, props: &str| {
        format!(
            "import type {{ Options }} from \"./types\";\nexport function scalar() {{\n\treturn {value};\n}}\nexport function Panel({props}) {{\n\treturn null;\n}}\n"
        )
    };
    repo.write(
        "src/panel.tsx",
        panel(
            "\"a\"",
            "{ options, legacy }: { options: Options; legacy?: string }",
        ),
    );
    repo.write(
        "src/screen.tsx",
        "import { Panel } from \"./panel\";\nexport function Screen() {\n\treturn <Panel options=\"a\" />;\n}\n",
    );
    repo.commit_all("initial");
    repo.write(
        "src/panel.tsx",
        panel("1", "{ options }: { options: Options }"),
    );
    assert_blocking(&repo, "Panel", "type through the declaring file");
}

#[test]
fn optional_props_removal_rejects_globals_declared_in_the_repository() {
    let with_options = |source: &str, ty: &str| {
        source.replace(
            "\tlabel: string;\n",
            &format!("\tlabel: string;\n\toptions?: {ty};\n"),
        )
    };
    // リポジトリの .d.ts (ignore していても) で宣言した大域の型は証明しない。
    for (path, declaration) in [
        (
            "types/globals.d.ts",
            "interface PanelOptions { value: string }\n",
        ),
        (
            "src/env.ts",
            "export {};\ndeclare global {\n\tinterface PanelOptions { value: string }\n}\n",
        ),
    ] {
        let repo = props_repo(
            &with_options(PANEL_BEFORE, "PanelOptions"),
            &with_options(PANEL_AFTER, "PanelOptions"),
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "types/\n"),
                (path, declaration),
            ],
        );
        assert_blocking(&repo, "Panel", path);
    }
    // 対照: lib の大域の型 (リポジトリで宣言していない) は互換。
    let repo = props_repo(
        &with_options(PANEL_BEFORE, "Record<string, string>"),
        &with_options(PANEL_AFTER, "Record<string, string>"),
        &[("src/screen.tsx", SCREEN)],
    );
    assert_compatible(&repo, "Panel", "lib global");
}

#[test]
fn optional_props_removal_rejects_package_bridges_outside_the_registry() {
    // node_modules の中のパッケージが、宣言を別名で再 export して、消したキーを渡す。
    let bridge = [
        (".gitignore", "node_modules/\n"),
        (
            "node_modules/bridge/index.d.ts",
            "export { Panel as Card } from \"../../src/panel\";\n",
        ),
        (
            "src/card.tsx",
            "import { Card } from \"bridge\";\nexport function CardScreen() {\n\treturn <Card label=\"Open\" legacy={{ id: \"a\" }} onDone={() => {}} />;\n}\n",
        ),
        ("src/screen.tsx", SCREEN),
    ];
    for extra in [
        ("packages/bridge/package.json", "{ \"name\": \"bridge\" }\n"),
        (
            "package.json",
            "{ \"name\": \"app\", \"dependencies\": { \"bridge\": \"file:./vendor/bridge\" } }\n",
        ),
        (
            "package.json",
            "{ \"name\": \"app\", \"dependencies\": { \"bridge\": \"workspace:*\" } }\n",
        ),
    ] {
        let mut files = bridge.to_vec();
        files.push(extra);
        let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &files);
        assert_blocking(&repo, "Panel", extra.1);
    }
    // 対照: registry の依存 (react) からの named import は、今までどおり受け入れる。
    let screen = SCREEN.replace(
        "import { Panel } from \"./panel\";\n",
        "import { Panel } from \"./panel\";\nimport { useState } from \"react\";\n",
    ) + "export const state = useState;\n";
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", &screen),
            (
                "package.json",
                "{ \"name\": \"app\", \"dependencies\": { \"react\": \"^18.2.0\" } }\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "registry dependency");
}

#[test]
fn optional_props_removal_audits_ignored_declaration_files() {
    for (path, source) in [
        (
            "types/env.d.ts",
            "declare namespace JSX {\n\tinterface IntrinsicAttributes {}\n\ttype LibraryManagedAttributes<C, P> = \"legacy\" extends keyof P ? P : P & { required: true };\n}\n",
        ),
        (
            "types/react.d.ts",
            "import \"react\";\ndeclare module \"react\" {\n\tinterface Attributes { required?: true }\n}\n",
        ),
        (
            "src/refs.ts",
            "/// <reference path=\"../node_modules/jsx-types/index.d.ts\" />\nexport {};\n",
        ),
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "types/\nnode_modules/\n"),
                (
                    "tsconfig.json",
                    "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" }, \"include\": [\"src\", \"types\"] }\n",
                ),
                ("node_modules/jsx-types/index.d.ts", "export {};\n"),
                (path, source),
            ],
        );
        assert_blocking(&repo, "Panel", path);
    }
    // 対照: ignore した .d.ts でも、JSX の宣言や React の augmentation が無ければ互換。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (".gitignore", "types/\n"),
            (
                "types/assets.d.ts",
                "declare module \"*.svg\" {\n\tconst url: string;\n\texport default url;\n}\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "ignored declarations without JSX");
}

#[test]
fn optional_props_removal_audits_ignored_alias_configs() {
    for (config, compatible) in [
        (
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"jsxImportSource\": \"preact\" }, \"include\": [\"src\"] }\n",
            false,
        ),
        (
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" }, \"include\": [\"src\"] }\n",
            true,
        ),
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "tsconfig.app.json\n"),
                (
                    "tsconfig.json",
                    "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" } }\n",
                ),
                ("tsconfig.app.json", config),
            ],
        );
        if compatible {
            assert_compatible(&repo, "Panel", config);
        } else {
            assert_blocking(&repo, "Panel", config);
        }
    }
}

#[test]
fn optional_props_removal_audits_ignored_tsconfig() {
    for (config, compatible) in [
        (
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"jsxImportSource\": \"preact\" } }\n",
            false,
        ),
        (
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" } }\n",
            true,
        ),
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "tsconfig.json\n"),
                ("tsconfig.json", config),
            ],
        );
        if compatible {
            assert_compatible(&repo, "Panel", config);
        } else {
            assert_blocking(&repo, "Panel", config);
        }
    }
}

#[test]
fn optional_props_removal_rejects_aliased_jsx_namespace_and_runtime_paths() {
    let runtime = (
        "src/runtime.ts",
        "namespace RuntimeTypes {\n\texport interface Element {}\n\texport type LibraryManagedAttributes<C, P> = \"legacy\" extends keyof P ? P : P & { required: true };\n}\nexport { RuntimeTypes as JSX };\n",
    );
    let runtime_paths = (
        "tsconfig.json",
        "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"paths\": { \"react/jsx-runtime\": [\"./src/runtime.ts\"] } } }\n",
    );
    for files in [
        vec![runtime, runtime_paths],
        vec![runtime],
        vec![runtime_paths],
        vec![
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"baseUrl\": \"./vendor\" } }\n",
            ),
            ("vendor/react/jsx-runtime.ts", "export {};\n"),
        ],
    ] {
        let mut files = files;
        files.push(("src/screen.tsx", SCREEN));
        let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &files);
        assert_blocking(&repo, "Panel", &format!("{files:?}"));
    }
    // 対照: React と無関係な paths だけなら互換。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"./src/*\"] } } }\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "unrelated paths");
}

const JSX_ENV: &str = "declare namespace JSX {\n\tinterface IntrinsicAttributes {}\n\ttype LibraryManagedAttributes<C, P> = \"legacy\" extends keyof P ? P : P & { required: true };\n}\n";

fn compatible_when(repo: &TestRepo, compatible: bool, context: &str) {
    if compatible {
        assert_compatible(repo, "Panel", context);
    } else {
        assert_blocking(repo, "Panel", context);
    }
}

#[test]
fn optional_props_removal_inventories_ignored_and_hidden_files() {
    let use_with = |call: &str| {
        format!(
            "import {{ Panel }} from \"../src/panel\";\nexport function Other() {{\n\treturn {call};\n}}\n"
        )
    };
    let supplied = use_with("<Panel label=\"x\" legacy={{ id: \"a\" }} onDone={() => {}} />");
    let omitted = use_with(CALL);
    // ignore したディレクトリと隠しディレクトリの中の利用も在庫に入れる。
    for (ignore, path) in [
        ((".ignore", "generated/\n"), "generated/bad.tsx"),
        ((".gitignore", "generated/\n"), "generated/bad.tsx"),
        ((".gitignore", "\n"), ".stories/bad.tsx"),
    ] {
        for (source, compatible) in [(&supplied, false), (&omitted, true)] {
            let repo = props_repo(
                PANEL_BEFORE,
                PANEL_AFTER,
                &[("src/screen.tsx", SCREEN), ignore, (path, source)],
            );
            compatible_when(&repo, compatible, &format!("{ignore:?} {path}: {source}"));
        }
    }
}

#[test]
fn optional_props_removal_tolerates_unrelated_ignored_files() {
    // shell の hook・.venv・target・名前の現れない大きなビルド出力 (512 ファイル・8 MiB を超える)
    // は、在庫に入れない。
    let mut files = vec![
        (
            ".gitignore".to_owned(),
            "dist/\n.venv/\ntarget/\n".to_owned(),
        ),
        (
            ".husky/pre-commit".to_owned(),
            "#!/bin/sh\nnpx lint-staged\n".to_owned(),
        ),
        (
            ".venv/lib/python3.12/site-packages/tool/__init__.py".to_owned(),
            "def run():\n    return 1\n".to_owned(),
        ),
        (
            "target/debug/build/out/bindings.rs".to_owned(),
            "pub fn run() {}\n".to_owned(),
        ),
        (
            "target/doc/search-index.js".to_owned(),
            "var searchIndex = new Map();\n".to_owned(),
        ),
        (
            "dist/vendor.js".to_owned(),
            format!("export const blob = \"{}\";\n", "a".repeat(9_000_000)),
        ),
        ("src/screen.tsx".to_owned(), SCREEN.to_owned()),
    ];
    files.extend((0..520).map(|index| {
        (
            format!("dist/chunks/chunk-{index}.js"),
            format!("export const value{index} = {index};\n"),
        )
    }));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &files);
    assert_compatible(&repo, "Panel", "unrelated ignored files");
}

#[test]
fn optional_props_removal_rejects_ignored_outputs_that_mention_the_name() {
    // 名前の現れる ignore したビルド出力 (コンパイル済みの `jsx(Panel, ...)`) は在庫に加え、
    // 証明できない出現として止まる。
    for source in [
        "import { jsx } from \"react/jsx-runtime\";\nimport { Panel } from \"../src/panel\";\nexport const view = jsx(Panel, { label: \"x\", legacy: { id: \"a\" }, onDone() {} });\n",
        "import{Panel as e}from\"../src/panel\";export const view=e;\n",
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "dist/\n"),
                ("dist/app.js", source),
            ],
        );
        assert_blocking(&repo, "Panel", source);
    }
}

#[test]
fn optional_props_removal_inventories_ignored_ts_that_import_the_module() {
    // 関数名を書かず、namespace import を計算したキーで使う ignore した TS は、宣言のファイルの
    // 名前 (`panel`) で在庫に加え、モジュール単位の検査で止まる。
    let namespace = "import * as m from \"../src/panel\";\nconst key = [\"Pa\", \"nel\"].join(\"\") as keyof typeof m;\nexport const view = m[key];\n";
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (".gitignore", "generated/\n"),
            ("generated/x.ts", namespace),
        ],
    );
    assert_blocking(&repo, "Panel", "namespace import with a computed key");
    // 対照: 宣言のファイルの名前も関数名も現れない ignore した TS は、在庫に入れない。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (".gitignore", "generated/\n"),
            (
                "generated/y.ts",
                "import * as s from \"../src/screen\";\nexport const other = s;\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "ignored TS without the module name");
}

#[test]
fn optional_props_removal_selects_ignored_ts_by_dependencies() {
    let namespace_use = |specifier: &str| {
        format!(
            "import * as m from \"{specifier}\";\nconst key = [\"Pa\", \"nel\"].join(\"\") as keyof typeof m;\nexport const use = m[key];\n"
        )
    };
    // ignore した宣言ファイルが、namespace import の型から props を使う (skipLibCheck: false
    // なら削除で型エラーになる)。
    let declaration = "import * as m from \"../src/panel\";\ntype Props = Parameters<(typeof m)[keyof typeof m]>[0];\nexport type Legacy = Props[\"legacy\"];\n";
    let paths = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"paths\": { \"@ui\": [\"./src/panel.tsx\"] } } }\n";
    let alias = namespace_use("@ui");
    for files in [
        vec![("generated/use.d.ts", declaration)],
        vec![("tsconfig.json", paths), ("generated/x.ts", alias.as_str())],
    ] {
        let mut files = files;
        files.extend([("src/screen.tsx", SCREEN), (".gitignore", "generated/\n")]);
        let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &files);
        assert_blocking(&repo, "Panel", &format!("{files:?}"));
    }
    // 対照: 宣言のファイルを import しない ignore した TS (registry と、ほかのファイルの import
    // だけ) は、在庫に入れない。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (".gitignore", "generated/\n"),
            (
                "generated/registry-only.ts",
                "import { useState } from \"react\";\nimport { helper } from \"./helper\";\nexport const both = [useState, helper];\n",
            ),
            ("generated/helper.ts", "export const helper = 1;\n"),
        ],
    );
    assert_compatible(
        &repo,
        "Panel",
        "ignored TS without a dependency on the target",
    );
}

#[test]
fn optional_props_removal_resolves_directory_imports_of_an_index_target() {
    // src/index.tsx の宣言を、ignore した src/ignored-use.ts が `.` から namespace import する
    // (名前も宣言のファイルの名前も現れない)。
    let repo_with = |ignored: Option<&str>| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write("src/index.tsx", PANEL_BEFORE);
        repo.write(
            "src/screen.tsx",
            SCREEN.replace("\"./panel\"", "\"./index\""),
        );
        repo.write(".gitignore", "src/ignored-use.ts\n");
        if let Some(source) = ignored {
            repo.write("src/ignored-use.ts", source);
        }
        repo.commit_all("initial");
        repo.write("src/index.tsx", PANEL_AFTER);
        repo
    };
    let dot = "import * as m from \".\";\nconst key = [\"Pa\", \"nel\"].join(\"\") as keyof typeof m;\nexport const use = m[key];\n";
    assert_blocking(&repo_with(Some(dot)), "Panel", "namespace import of `.`");
    assert_compatible(
        &repo_with(None),
        "Panel",
        "index target without the ignored use",
    );
}

#[test]
fn optional_props_removal_resolves_npm_override_references() {
    // overrides の `$donor` は、donor の指定 (registry でない) を参照する。bridge は宣言を別名で
    // 再 export して、消したキーを渡す。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (".gitignore", "node_modules/\n"),
            (
                "package.json",
                "{ \"name\": \"app\", \"dependencies\": { \"donor\": \"file:./vendor/donor\", \"bridge\": \"^1.0.0\" }, \"overrides\": { \"bridge\": \"$donor\" } }\n",
            ),
            ("vendor/donor/package.json", "{ \"name\": \"donor\" }\n"),
            (
                "node_modules/bridge/index.d.ts",
                "export { Panel as Card } from \"../../src/panel\";\n",
            ),
            (
                "src/use.tsx",
                "import { Card } from \"bridge\";\nexport function Use() {\n\treturn <Card label=\"Open\" legacy={{ id: \"a\" }} onDone={() => {}} />;\n}\n",
            ),
        ],
    );
    assert_blocking(&repo, "Panel", "override referencing a local donor");
}

#[test]
fn optional_props_removal_reads_package_manager_configs() {
    // pnpm-workspace.yaml の overrides がローカルの依存に差し替えた bridge が、宣言を別名で再
    // export して、消したキーを渡す。
    let repo_with = |yaml: &str| {
        props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "node_modules/\n"),
                (
                    "package.json",
                    "{ \"name\": \"app\", \"dependencies\": { \"bridge\": \"^1.0.0\" } }\n",
                ),
                ("pnpm-workspace.yaml", yaml),
                ("vendor/bridge/README.md", "# local bridge\n"),
                (
                    "node_modules/bridge/index.d.ts",
                    "export { Panel as Card } from \"../../src/panel\";\n",
                ),
                (
                    "src/use.tsx",
                    "import { Card } from \"bridge\";\nexport function Use() {\n\treturn <Card label=\"Open\" legacy={{ id: \"a\" }} onDone={() => {}} />;\n}\n",
                ),
            ],
        )
    };
    for yaml in [
        "overrides:\n  bridge: link:../bridge\n",
        "overrides:\n  bridge: file:./vendor/bridge\n",
    ] {
        assert_blocking(&repo_with(yaml), "Panel", yaml);
    }
    // 対照: registry の版だけを指定する YAML は、Issue の形を互換のまま残す。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (
                "pnpm-workspace.yaml",
                "packages:\n  - packages/*\noverrides:\n  react: ^18.3.0\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "registry versions in the YAML");
}

#[test]
fn optional_props_removal_treats_package_directories_as_undetermined() {
    // ui/package.json の types が宣言のファイルを指す。ignore した gallery.tsx は、ディレクトリの
    // import と計算した要素で、消したキーを渡す (名前も宣言のファイルの名前も現れない)。
    let repo_with = |gallery: &str| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("ui");
        repo.create_dir_all("src");
        repo.create_dir_all("generated");
        repo.create_dir_all("lib");
        repo.write("ui/components.tsx", PANEL_BEFORE);
        repo.write("ui/package.json", "{ \"types\": \"./components.tsx\" }\n");
        repo.write(
            "src/screen.tsx",
            SCREEN.replace("\"./panel\"", "\"../ui/components\""),
        );
        repo.write("lib/helpers.ts", "export const helper = 1;\n");
        repo.write(".gitignore", "generated/\n");
        repo.write("generated/gallery.tsx", gallery);
        repo.commit_all("initial");
        repo.write("ui/components.tsx", PANEL_AFTER);
        repo
    };
    assert_blocking(
        &repo_with(
            "import * as UI from \"../ui\";\nexport const gallery = Object.values(UI).map((C) => <C label=\"demo\" legacy=\"old\" />);\n",
        ),
        "Panel",
        "directory import through package.json",
    );
    // 対照: 宣言のファイルに届かない通常の相対 import は、在庫に入れない。
    assert_compatible(
        &repo_with("import { helper } from \"../lib/helpers\";\nexport const gallery = helper;\n"),
        "Panel",
        "relative import of another file",
    );
}

#[test]
fn optional_props_removal_rejects_module_suffixes() {
    // `moduleSuffixes: [".native", ""]` では、`@views` (`./src/screen`) は宣言のある
    // src/screen.native.tsx に解決される。ignore した gallery.tsx は計算した要素で、消したキーを
    // 渡す。この解決器では moduleSuffixes を解けないので、監査を作らない。
    let repo_with = |suffixes: &str| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.create_dir_all("generated");
        repo.write(
            "tsconfig.json",
            format!(
                "{{ \"compilerOptions\": {{ \"jsx\": \"react-jsx\", \"moduleSuffixes\": {suffixes}, \"paths\": {{ \"@views\": [\"./src/screen\"] }} }} }}\n"
            ),
        );
        repo.write("src/screen.native.tsx", PANEL_BEFORE);
        repo.write("src/screen.tsx", "export const other = 1;\n");
        repo.write(
            "src/app.tsx",
            "import { Panel } from \"./screen.native\";\nexport function App() {\n\treturn <Panel label=\"Open\" onDone={() => {}} />;\n}\n",
        );
        repo.write(".gitignore", "generated/\n");
        repo.write(
            "generated/gallery.tsx",
            "import * as UI from \"@views\";\nexport const gallery = Object.values(UI).map((C) => <C label=\"demo\" legacy=\"old\" />);\n",
        );
        repo.commit_all("initial");
        repo.write("src/screen.native.tsx", PANEL_AFTER);
        repo
    };
    assert_blocking(
        &repo_with("[\".native\", \"\"]"),
        "Panel",
        "module suffixes",
    );
    // 対照: `[""]` は解決先を変えない。
    assert_compatible(&repo_with("[\"\"]"), "Panel", "empty module suffix");
}

#[test]
fn optional_props_removal_resolves_specifiers_that_leave_and_reenter_the_root() {
    // ignore した宣言ファイルが、解析ルートの外へ一度出て戻る相対パスで宣言を再 export する。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[("src/screen.tsx", SCREEN), (".gitignore", "generated/\n")],
    );
    let root = repo.root().canonicalize().unwrap();
    let name = root.file_name().unwrap().to_str().unwrap();
    repo.create_dir_all("generated");
    repo.write(
        "generated/exports.d.ts",
        format!("export * from \"../../{name}/src/panel\";\n"),
    );
    assert_blocking(&repo, "Panel", "specifier through the parent directory");
}

#[test]
fn optional_props_removal_rejects_pnpmfile_settings_in_npmrc() {
    // `.npmrc` の pnpmfile は、別の名前の hook で依存の指定を書き換え得る。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (".npmrc", "pnpmfile=scripts/install-hooks.cjs\n"),
        ],
    );
    assert_blocking(&repo, "Panel", "pnpmfile in .npmrc");
}

#[test]
fn optional_props_removal_requires_registry_react_runtime() {
    // 自動の runtime の import はソースに現れない。React (と型の @types/react) の出どころを
    // package.json で確かめる。
    for jsx in ["react-jsx", "react-jsxdev"] {
        let config = format!("{{ \"compilerOptions\": {{ \"jsx\": \"{jsx}\" }} }}\n");
        for (dependencies, compatible) in [
            ("\"react\": \"file:../custom-react\"", false),
            ("\"react\": \"link:./vendor/react\"", false),
            ("\"react\": \"npm:@preact/compat@^7\"", false),
            (
                "\"react\": \"^18.2.0\", \"@types/react\": \"file:./vendor/types\"",
                false,
            ),
            (
                "\"react\": \"^18.2.0\", \"@types/react\": \"^18.2.0\"",
                true,
            ),
        ] {
            let manifest =
                format!("{{ \"name\": \"app\", \"dependencies\": {{ {dependencies} }} }}\n");
            let repo = props_repo(
                PANEL_BEFORE,
                PANEL_AFTER,
                &[
                    ("src/screen.tsx", SCREEN),
                    ("tsconfig.json", &config),
                    ("package.json", &manifest),
                    ("vendor/react/index.js", "export const jsx = null;\n"),
                    ("vendor/types/index.d.ts", "export {};\n"),
                ],
            );
            compatible_when(&repo, compatible, &format!("{jsx} {dependencies}"));
        }
    }
}

#[test]
fn optional_props_removal_checks_every_package_manifest() {
    // node_modules の bridge が宣言を別名で再 export して、消したキーを渡す。use.tsx に近い
    // src/package.json は bridge を指定しない。
    let bridge = [
        ("src/screen.tsx", SCREEN),
        (".gitignore", "node_modules/\n"),
        (
            "node_modules/bridge/index.d.ts",
            "export { Panel as Card } from \"../../src/panel\";\n",
        ),
        (
            "src/use.tsx",
            "import { Card } from \"bridge\";\nexport function Use() {\n\treturn <Card label=\"Open\" legacy={{ id: \"a\" }} onDone={() => {}} />;\n}\n",
        ),
        ("src/package.json", "{ \"name\": \"app\" }\n"),
    ];
    for manifests in [
        vec![(
            "package.json",
            "{ \"name\": \"root\", \"dependencies\": { \"bridge\": \"file:../bridge\" } }\n",
        )],
        vec![
            (
                "package.json",
                "{ \"name\": \"root\", \"workspaces\": [\"src\", \"packages/*\"] }\n",
            ),
            (
                "packages/other/package.json",
                "{ \"name\": \"other\", \"dependencies\": { \"bridge\": \"link:../../../bridge\" } }\n",
            ),
        ],
        vec![(
            "package.json",
            "{ \"name\": \"root\", \"dependencies\": { \"bridge\": \"^1.0.0\" }, \"overrides\": { \"bridge\": \"file:../bridge\" } }\n",
        )],
    ] {
        let mut files = bridge.to_vec();
        files.extend(manifests.iter().copied());
        let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &files);
        assert_blocking(&repo, "Panel", &format!("{manifests:?}"));
    }
    // 対照: 祖先の package.json の registry の依存 (react) からの named import は受け入れる。
    let screen = SCREEN.replace(
        "import { Panel } from \"./panel\";\n",
        "import { Panel } from \"./panel\";\nimport { useState } from \"react\";\n",
    ) + "export const state = useState;\n";
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", &screen),
            ("src/package.json", "{ \"name\": \"app\" }\n"),
            (
                "package.json",
                "{ \"name\": \"root\", \"dependencies\": { \"react\": \"^18.2.0\" } }\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "registry dependency of an ancestor");
}

#[cfg(unix)]
#[test]
fn optional_props_removal_rejects_directory_symlinks() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("env.d.ts"), JSX_ENV).unwrap();
    let config = (
        "tsconfig.json",
        "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" }, \"files\": [\"types/env.d.ts\", \"src/panel.tsx\", \"src/screen.tsx\"] }\n",
    );
    for target in [
        outside.path().to_path_buf(),
        std::path::PathBuf::from("node_modules/type-env"),
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "node_modules/\n"),
                ("node_modules/type-env/env.d.ts", JSX_ENV),
                config,
            ],
        );
        let target = if target.is_absolute() {
            target
        } else {
            repo.path(target)
        };
        std::os::unix::fs::symlink(&target, repo.path("types")).unwrap();
        assert_blocking(&repo, "Panel", &format!("types -> {target:?}"));
    }
    // 対照: 在庫・監査の対象にならないファイルへの symlink は許す。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[("src/screen.tsx", SCREEN), ("docs/guide.md", "# guide\n")],
    );
    std::os::unix::fs::symlink(repo.path("docs/guide.md"), repo.path("README.md")).unwrap();
    assert_compatible(&repo, "Panel", "file symlink to a document");
}

#[test]
fn optional_props_removal_reads_reference_attributes() {
    // 属性名・空白 (タブを含む)・`=`・どちらの引用符も、TypeScript と同じく読む。
    for (directive, compatible) in [
        (
            "/// <reference path = \"../../type-env/env.d.ts\" />",
            false,
        ),
        (
            "/// <reference path\t=\t'../../type-env/env.d.ts' />",
            false,
        ),
        (
            "/// <REFERENCE PATH=\"../node_modules/jsx-types/index.d.ts\" />",
            false,
        ),
        ("/// <reference path = \"./env.d.ts\" />", true),
        ("/// <reference path\t=\t'./env.d.ts' />", true),
    ] {
        let repo = props_repo(
            PANEL_BEFORE,
            PANEL_AFTER,
            &[
                ("src/screen.tsx", SCREEN),
                (".gitignore", "node_modules/\n"),
                ("node_modules/jsx-types/index.d.ts", JSX_ENV),
                ("src/env.d.ts", "export {};\n"),
                ("src/refs.ts", &format!("{directive}\nexport {{}};\n")),
            ],
        );
        compatible_when(&repo, compatible, directive);
    }
}

#[test]
fn optional_props_removal_rejects_types_brought_in_from_outside() {
    // 設定・reference・pragma が、監査していない JSX の型環境を持ち込む。
    for files in [
        vec![(
            "tsconfig.json",
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" }, \"files\": [\"../type-env/env.d.ts\", \"src/panel.tsx\", \"src/screen.tsx\"] }\n",
        )],
        vec![(
            "tsconfig.json",
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" }, \"include\": [\"src\", \"../type-env\"] }\n",
        )],
        vec![(
            "tsconfig.json",
            "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"typeRoots\": [\"../type-roots\"] } }\n",
        )],
        vec![
            (
                "package.json",
                "{ \"name\": \"app\", \"devDependencies\": { \"custom-jsx\": \"file:./vendor/custom-jsx\" } }\n",
            ),
            ("vendor/custom-jsx/index.d.ts", "export {};\n"),
            (
                "src/env.ts",
                "/// <reference types=\"custom-jsx\" />\nexport {};\n",
            ),
        ],
        vec![(
            "src/other.tsx",
            "/** @JSXImportSource preact */\nexport const view = <div />;\n",
        )],
    ] {
        let mut files = files;
        files.push(("src/screen.tsx", SCREEN));
        let repo = props_repo(PANEL_BEFORE, PANEL_AFTER, &files);
        assert_blocking(&repo, "Panel", &format!("{files:?}"));
    }
    // 対照: 中だけを取り込む設定と、registry の型のパッケージは互換のまま。
    let repo = props_repo(
        PANEL_BEFORE,
        PANEL_AFTER,
        &[
            ("src/screen.tsx", SCREEN),
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"typeRoots\": [\"./node_modules/@types\", \"./types\"], \"types\": [\"node\"] }, \"include\": [\"src\", \"types\"] }\n",
            ),
            ("types/assets.d.ts", "declare module \"*.svg\";\n"),
            (
                "src/env.ts",
                "/// <reference types=\"node\" />\nexport {};\n",
            ),
        ],
    );
    assert_compatible(&repo, "Panel", "types inside the repository");
}
