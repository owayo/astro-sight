use super::support::{TestRepo, cargo_bin};

fn review_default(
    prelude: &str,
    ty: &str,
    value: &str,
) -> (std::process::Output, serde_json::Value) {
    run_review(
        &default_repo(prelude, prelude, &format!("tone?: {ty}"), value, &[]),
        false,
    )
}

fn default_repo(
    old_prelude: &str,
    prelude: &str,
    prop: &str,
    value: &str,
    files: &[(&str, &str)],
) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tone.ts", "export type Tone = 'plain' | 'muted';\n");
    repo.write("panel.tsx", format!("{old_prelude}\nexport function Panel({{ label }}: {{ label: string }}) {{ return <p>{{label}}</p>; }}\n"));
    repo.write("home.tsx", "import { Panel } from './panel';\nexport function Home() { return <Panel label='Name' />; }\n");
    for (path, text) in files {
        repo.write(path, text);
    }
    repo.commit_all("initial");
    repo.write("panel.tsx", format!("{prelude}\nexport function Panel({{ label, tone = {value} }}: {{ label: string; {prop} }}) {{ return <p data-tone={{tone}}>{{label}}</p>; }}\n"));
    repo
}

fn run_review(repo: &TestRepo, staged: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = cargo_bin();
    command.args(["review", "--git", "--hook"]);
    if staged {
        command.arg("--staged");
    }
    let output = command.arg("--dir").arg(repo.root()).output().unwrap();
    let result = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "hook JSON: {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, result)
}

fn compatible(result: &serde_json::Value) -> bool {
    result["api"]["mod_compat"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["n"] == "Panel" && item["reason"] == "optional_object_props")
}

#[test]
fn optional_prop_additions_check_inferred_returns_and_outer_bindings() {
    let before = "{ label }: { label: string }";
    let added = "{ label, tone }: { label: string; tone?: 'plain' | 'muted' }";
    let direct = "Panel({ label: 'a' })";
    let mut failures = Vec::new();
    for (case, new_param, old_body, new_body, caller, safe) in [
        (
            "outer binding",
            added,
            "return tone;",
            "return tone;",
            direct,
            false,
        ),
        (
            "new return",
            added,
            "return label.length;",
            "return tone;",
            direct,
            false,
        ),
        (
            "nested capture",
            added,
            "return () => tone;",
            "return () => tone;",
            direct,
            false,
        ),
        (
            "shorthand capture",
            added,
            "return {tone};",
            "return {tone};",
            direct,
            false,
        ),
        (
            "computed method name precedes its parameters",
            added,
            "return { [tone](tone: string) { return tone; } };",
            "return { [tone](tone: string) { return tone; } };",
            direct,
            false,
        ),
        (
            "branch literal",
            added,
            "if (label) return 'a'; return 'b';",
            "if (label) return 'a'; return 'c';",
            direct,
            false,
        ),
        (
            "return newline",
            added,
            "return 1;",
            "return\n1;",
            direct,
            false,
        ),
        (
            "generator",
            added,
            "yield 1; return true;",
            "yield 's'; return true;",
            direct,
            false,
        ),
        (
            "pragma",
            added,
            "return <p>{label}</p>;",
            "return <p>{label}</p>;",
            direct,
            false,
        ),
        (
            "removed pragma",
            added,
            "return <p>{label}</p>;",
            "return <p>{label}</p>;",
            direct,
            false,
        ),
        (
            "return newline before JSX",
            added,
            "return (<p>{label}</p>);",
            "return\n(<p>{label}</p>);",
            direct,
            false,
        ),
        (
            "prototype",
            "{ label }: { label: string; toString?: 'plain' }",
            "return label.length;",
            "return label.length;",
            direct,
            false,
        ),
        (
            "JSX key",
            "{ label }: { label: string; key?: 'plain' }",
            "return <p>{label}</p>;",
            "return <p>{label}</p>;",
            "<Panel label='a' key={1} />",
            false,
        ),
        (
            "old parameter",
            added,
            "return label.length;",
            "return label.length;",
            direct,
            true,
        ),
        (
            "property name",
            added,
            "return {tone: label};",
            "return {tone: label};",
            direct,
            true,
        ),
        (
            "conditional JSX",
            added,
            "return <p>{label}</p>;",
            "return tone ? <p>{label}</p> : (<b>{label}</b>);",
            direct,
            true,
        ),
        (
            "conditional non-JSX branch",
            added,
            "return <p>{label}</p>;",
            "return tone ? <p>{label}</p> : null;",
            direct,
            false,
        ),
        (
            "JSX body",
            added,
            "return <p>{label}</p>;",
            "return <p data-tone={tone}>{label}</p>;",
            direct,
            true,
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let declaration = |param: &str, body: &str, pragma: &str| {
            let star = if case == "generator" { "*" } else { "" };
            format!(
                "{pragma}const tone = 123;\nexport function{star} Panel({param}) {{ {body} }}\n"
            )
        };
        let pragma = "/** @jsxImportSource other */\n";
        let old_pragma = if case == "removed pragma" { pragma } else { "" };
        repo.write("panel.tsx", declaration(before, old_body, old_pragma));
        repo.write(
            "home.tsx",
            format!("import {{ Panel }} from './panel';\nexport const n = {caller};\n"),
        );
        repo.commit_all("base");
        let new_pragma = if case == "pragma" { pragma } else { "" };
        repo.write("panel.tsx", declaration(new_param, new_body, new_pragma));
        let (output, result) = run_review(&repo, false);
        let modified = result["api"]["mod"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|m| m["n"] == "Panel");
        if output.status.success() != safe || compatible(&result) != safe || modified == safe {
            failures.push(format!("{case}: {result}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn optional_prop_additions_check_returned_props_in_the_index() {
    let cases = [
        ("whole", "props", "", "return props;", false),
        (
            "throughDefault",
            "props",
            ", value = props",
            "return value;",
            false,
        ),
        ("rest", "{ ...props }", "", "return props;", false),
        ("projection", "props", "", "return props.label;", true),
        ("fixed", "props", "", "return 3;", true),
    ];
    let declarations = |ty: &str| {
        cases
            .iter()
            .map(|(name, binding, tail, body, _)| {
                format!("export function {name}({binding}: {ty}{tail}) {{ {body} }}\n")
            })
            .collect::<String>()
    };
    let repo = TestRepo::new();
    repo.init_git();
    let before = declarations("{ label: string }");
    repo.write("api.ts", &before);
    let names = cases
        .iter()
        .map(|case| case.0)
        .collect::<Vec<_>>()
        .join(", ");
    let mut caller = format!("import {{ {names} }} from './api';\n");
    for (name, ..) in cases {
        caller.push_str(&format!(
            "export const {name}Result = {name}({{ label: 'a' }});\n"
        ));
    }
    caller.push_str(
        "export const checked: keyof typeof wholeResult extends 'label' ? true : false = true;\n",
    );
    repo.write("home.ts", caller);
    repo.commit_all("base");
    repo.write("api.ts", declarations("{ label: string; tone?: string }"));
    repo.stage_all();
    repo.write("api.ts", before);
    let (output, result) = run_review(&repo, true);
    assert_eq!(output.status.code(), Some(1), "{result}");
    for (name, _, _, _, safe) in cases {
        let contains = |bucket: &str| {
            result["api"][bucket]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m["n"] == name)
        };
        assert_eq!(contains("mod_compat"), safe, "{name}: {result}");
        assert_eq!(contains("mod"), !safe, "{name}: {result}");
    }
}

#[test]
fn optional_prop_additions_check_each_caller_in_the_reviewed_snapshot() {
    let before = "export function Panel({ label }: { label: string }) { return <p>{label}</p>; }\n";
    let after = before.replace(
        "{ label: string }",
        "{ label: string; tone?: 'plain' | 'muted' }",
    );
    for (old_call, new_call, safe) in [
        ("<Panel {...props} />", "<Panel {...props} />", false),
        ("Panel(props)", "Panel(props)", false),
        ("Panel({ ...props })", "Panel({ ...props })", false),
        ("Panel", "Panel", false),
        (
            "(\n// @ts-expect-error\nPanel({ label: 'a', tone: 'plain' }))",
            "(\n// @ts-expect-error\nPanel({ label: 'a', tone: 'plain' }))",
            false,
        ),
        ("Panel(props)", "Panel({ ...props, label: 'b' })", false),
        (
            "Panel({ label: 'a' })",
            "Panel({ label: 'b', tone: 1 })",
            false,
        ),
        (
            "Panel({ label: 'a' })",
            "Panel({ label: 'b', tone: 'plain' })",
            true,
        ),
        ("<Panel label='a' />", "<Panel label='a' />", true),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("panel.tsx", before);
        let caller = |call: &str| {
            format!(
                "import {{ Panel }} from './panel';\nconst props = {{ label: 'a', tone: 123 }};\n{call};\n"
            )
        };
        repo.write("home.tsx", caller(old_call));
        repo.commit_all("base");
        repo.write("panel.tsx", &after);
        repo.write("home.tsx", caller(new_call));
        repo.stage_all();
        for staged in [false, true] {
            if staged {
                repo.write(
                    "home.tsx",
                    caller(if safe {
                        "Panel(props)"
                    } else {
                        "<Panel label='a' />"
                    }),
                );
            }
            let (output, result) = run_review(&repo, staged);
            assert_eq!(
                output.status.code(),
                Some(i32::from(!safe)),
                "{new_call}/{staged}: {result}"
            );
            assert_eq!(compatible(&result), safe, "{new_call}: {result}");
            assert_eq!(
                result["api"]["mod"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|item| item["n"] == "Panel"),
                !safe,
                "{new_call}: {result}"
            );
        }
    }
}

#[test]
fn optional_prop_additions_check_defaults_and_this_parameter_positions() {
    for (parameter, call, safe) in [
        ("p: { label: string } = props", "Panel()", false),
        ("p: { label: string } | string", "Panel('old')", true),
        ("p: { label: string } | null", "Panel(null)", true),
        ("p: { label: string } = { label: 'x' }", "Panel()", true),
        ("this: void, p: { label: string }", "Panel(props)", false),
        (
            "this: void, p: { label: string }",
            "Panel({label:'x'})",
            true,
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let declaration = |param: &str| {
            format!(
                "const props = {{ label: 'x', tone: 123 }};\nexport function Panel({param}) {{ return 0; }}\n"
            )
        };
        repo.write("panel.ts", declaration(parameter));
        repo.write("home.ts", format!("import {{ Panel }} from './panel';\nconst props = {{ label: 'x', tone: 123 }};\nexport const home = {call};\n"));
        repo.commit_all("base");
        repo.write(
            "panel.ts",
            declaration(&parameter.replace("label: string", "label: string; tone?: string")),
        );
        let (output, result) = run_review(&repo, false);
        assert_eq!(
            output.status.code(),
            Some(i32::from(!safe)),
            "{parameter}/{call}: {result}"
        );
        assert_eq!(compatible(&result), safe, "{result}");
    }
}

#[test]
fn optional_prop_additions_account_for_inherited_keys() {
    for (key, globals, safe) in [
        ("tone", "", true),
        ("toString", "", false),
        ("tone", "interface Object { tone: number }", false),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let before = "export function Panel(p: { label: string }) { return p.label; }\n";
        repo.write("panel.ts", before);
        repo.write("globals.d.ts", globals);
        repo.write(
            "home.ts",
            "import { Panel } from './panel';\nPanel({ label: 'x' });\n",
        );
        repo.commit_all("base");
        repo.write(
            "panel.ts",
            before.replace("label: string", &format!("label: string; {key}?: string")),
        );
        let (output, result) = run_review(&repo, false);
        assert_eq!(
            output.status.code(),
            Some(i32::from(!safe)),
            "{key}/{globals}: {result}"
        );
        assert_eq!(compatible(&result), safe, "{result}");
    }
}

#[test]
fn optional_prop_additions_check_named_function_expression_recursion() {
    for (argument, safe) in [("bad", false), ("{label:'x'}", true)] {
        let repo = TestRepo::new();
        repo.init_git();
        let before = format!(
            "const bad = {{label:'x',tone:1}};\nexport const Panel = function inner(p: {{label:string}}): number {{ return p.label==='x'?1:inner({argument}); }};\n"
        );
        repo.write("panel.ts", &before);
        repo.write(
            "home.ts",
            "import { Panel } from './panel';\nPanel({label:'x'});\n",
        );
        repo.commit_all("base");
        repo.write(
            "panel.ts",
            before.replace("label:string", "label:string;tone?:string"),
        );
        let (output, result) = run_review(&repo, false);
        assert_eq!(
            output.status.code(),
            Some(i32::from(!safe)),
            "{argument}: {result}"
        );
        assert_eq!(compatible(&result), safe, "{result}");
    }
}

#[test]
fn unannotated_object_parameters_report_inferred_contract_changes() {
    for (extension, before, after, argument, changes) in [
        (
            "ts",
            "{ label = '' }",
            "{ label = 0 }",
            Some("{label: 'first'}"),
            true,
        ),
        (
            "tsx",
            "{ label = '' }",
            "{ label = '', size }",
            Some("{}"),
            true,
        ),
        ("ts", "{ label = '' }", "{ label }", None, true),
        (
            "ts",
            "{ key: { value = true } }",
            "{ key: { value = 1 } }",
            Some("{key: {value: true}}"),
            true,
        ),
        (
            "ts",
            "{ label = 'a b' as const }",
            "{ label = 'a  b' as const }",
            Some("{label: 'a b'}"),
            true,
        ),
        (
            "ts",
            "{ label: local = '' }, size = fallback",
            "{ label: fallback = '' }, size = fallback",
            Some("{}, 5"),
            true,
        ),
        (
            "ts",
            "{ label: local = '', size = fallback }",
            "{ label: fallback = '', size = fallback }",
            Some("{size: 5}"),
            true,
        ),
        (
            "ts",
            "{ label: x = '' }",
            "{ /* note */ label: y = 'next' }",
            Some("{}"),
            false,
        ),
        (
            "tsx",
            "{ size = 1 }",
            "{ size = 3 }",
            Some("{size: 5}"),
            false,
        ),
        (
            "ts",
            "tag: string, { label = 0 }",
            "tag: string, { /* note */ label = 0 }",
            Some("'tag', {}"),
            false,
        ),
        (
            "ts",
            "{ box: { label = 0 } }",
            "{\n box:\n { label = 0 }\n}",
            Some("{box: {}}"),
            false,
        ),
        (
            "ts",
            "{ key = Number('1') }",
            "{ /* note */ key = Number('1') }",
            Some("{}"),
            false,
        ),
        (
            "ts",
            "{ key = Number('1') }",
            "{ key = Number( /* note */ '1' ) }",
            Some("{}"),
            false,
        ),
        (
            "ts",
            "{ key = 'x' as const }",
            "{ key = 'x'  as  const }",
            Some("{}"),
            false,
        ),
        (
            "ts",
            "{ key = () => { return 1; } }",
            "{ key = () => { return\n1; } }",
            Some("{}"),
            true,
        ),
        (
            "ts",
            "{ label }: { label: string }",
            "{ label: item }: { label: string }",
            Some("{label: 'a'}"),
            false,
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let path = format!("panel.{extension}");
        let write = |parameter: &str| {
            repo.write(
                &path,
                format!(
                    "const fallback = 7;\nexport function Panel({parameter}) {{ return 0; }}\n"
                ),
            )
        };
        write(before);
        if let Some(argument) = argument {
            repo.write(
                "home.ts",
                format!(
                    "import {{ Panel }} from './panel';\nexport const home = Panel({argument});\n"
                ),
            );
        }
        repo.commit_all("base");
        write(after);
        let review = repo.run_json("review", &["--git"]);
        repo.stage_all();
        write(before);
        let (output, hook) = run_review(&repo, true);
        let modified = review["api_changes"]["modified"].as_array().unwrap();
        if changes {
            assert_eq!(output.status.code(), Some(1), "{before} -> {after}: {hook}");
            assert_eq!(
                hook["api"]["mod"][0]["n"], "Panel",
                "{before} -> {after}: {hook}"
            );
            assert_eq!(modified[0]["name"], "Panel");
            assert_ne!(modified[0]["old_signature"], modified[0]["new_signature"]);
        } else {
            assert!(modified.is_empty(), "{before} -> {after}: {review}");
            assert!(hook["api"].is_null(), "{hook}");
        }
    }
}

#[test]
fn destructured_optional_defaults_literal_unions_are_compatible() {
    for (ty, value) in [("'plain' | 'muted'", "'plain'"), ("1 | 2", "1")] {
        let (output, result) = review_default("", ty, value);
        assert!(output.status.success(), "{ty}: {result}");
        assert!(compatible(&result), "{ty}: {result}");
        assert!(result["impacts"].is_null(), "{ty}: {result}");
        assert!(result["impact_info"].is_array(), "{ty}: {result}");
    }
}

#[test]
fn destructured_optional_defaults_local_alias_is_compatible() {
    let (output, result) = review_default("type Tone = 'plain' | 'muted';", "Tone", "'plain'");
    assert!(output.status.success(), "{result}");
    assert!(compatible(&result), "{result}");
    assert!(result["impacts"].is_null(), "{result}");
}

#[test]
fn destructured_optional_defaults_unknown_and_mismatched_types_stay_blocking() {
    for (prelude, ty, value) in [
        ("", "'plain' | 'muted'", "'loud'"),
        ("import type { Tone } from './tone';", "Tone", "'loud'"),
        ("import type { Tone } from './missing';", "Tone", "'plain'"),
        ("import { Tone } from './tone';", "Tone", "'plain'"),
    ] {
        let (output, result) = review_default(prelude, ty, value);
        assert_eq!(output.status.code(), Some(1), "{prelude}/{ty}: {result}");
        assert!(!compatible(&result), "{prelude}/{ty}: {result}");
        assert!(
            result["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["n"] == "Panel"),
            "{prelude}/{ty}: {result}"
        );
    }
}

const INDEXED: &str = "const tones = [{ value: 'plain' }, { value: 'muted' }] as const;\ntype Tone = (typeof tones)[number]['value'];";

#[test]
fn destructured_optional_defaults_prove_imported_alias_and_indexed_literals() {
    for (prelude, prop, value) in [
        (
            "import type { Tone } from './tone';",
            "tone?: Tone",
            "'plain'",
        ),
        (
            "import type { Tone as T } from './tone.js';",
            "tone?: T",
            "\"muted\"",
        ),
        (
            "import type { Tone } from './tone.ts';",
            "tone?: Tone | null",
            "null",
        ),
        (INDEXED, "tone?: Tone", "\"plain\""),
    ] {
        let repo = default_repo("", prelude, prop, value, &[]);
        repo.stage_all();
        for staged in [false, true] {
            if staged {
                repo.write("tone.ts", "export type Tone = 'other';");
                repo.write("panel.tsx", "export {};");
            }
            let (output, result) = run_review(&repo, staged);
            assert!(output.status.success(), "{prelude}/{staged}: {result}");
            assert!(compatible(&result), "{prelude}/{staged}: {result}");
            assert!(result["impacts"].is_null(), "{result}");
        }
    }
}

#[test]
fn destructured_optional_defaults_keep_unproven_projections_and_required_props_blocking() {
    for (prelude, prop, value) in [
        (INDEXED.to_owned(), "tone?: Tone", "'loud'"),
        (
            INDEXED.replace("value: 'muted'", "name: 'muted'"),
            "tone?: Tone",
            "'plain'",
        ),
        (
            INDEXED.replace("value: 'muted'", "...makeTone()"),
            "tone?: Tone",
            "'plain'",
        ),
        (INDEXED.to_owned(), "tone: Tone", "'plain'"),
        (
            "import type { Tone } from './tone';".to_owned(),
            "tone: Tone",
            "'plain'",
        ),
    ] {
        let (output, result) = run_review(&default_repo("", &prelude, prop, value, &[]), false);
        assert_eq!(output.status.code(), Some(1), "{prelude}/{prop}: {result}");
        assert!(!compatible(&result), "{result}");
    }
}

#[test]
fn destructured_optional_defaults_require_one_exported_type_at_the_import_target() {
    for (path, text) in [
        ("tone.ts", "type Tone = 'plain'; export {};"),
        ("tone.ts", "export type Tone<T> = 'plain';"),
        ("tone.ts", "export type Tone = Missing | 'plain';"),
        ("tone.d.ts", "export type Tone = 'other';"),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"moduleSuffixes":[".native",""]}}"#,
        ),
    ] {
        let repo = default_repo(
            "",
            "import type { Tone } from './tone';",
            "tone?: Tone",
            "'plain'",
            &[(path, text)],
        );
        let (output, result) = run_review(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{path}/{text}: {result}");
        assert!(
            result["api"]["mod"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["n"] == "Panel"),
            "{result}"
        );
    }
}

#[test]
fn destructured_optional_defaults_require_erased_import_for_effectful_module() {
    for (prelude, safe) in [
        ("import { type Tone } from './tone';", false),
        ("import type { Tone } from './tone';", true),
    ] {
        let repo = default_repo(
            "",
            prelude,
            "tone?: Tone",
            "'plain'",
            &[(
                "tone.ts",
                "console.log('loaded');\nexport type Tone = 'plain' | 'muted';",
            )],
        );
        let (output, result) = run_review(&repo, false);
        assert_eq!(output.status.code(), Some(i32::from(!safe)), "{result}");
        assert_eq!(compatible(&result), safe, "{result}");
    }
}

#[test]
fn private_named_props_check_optional_keys_binding_and_repository_uses() {
    let omit = "<Panel label='a' />";
    let mut failures = Vec::new();
    for (declaration, member, body, observer, caller, safe) in [
        (
            "interface Props",
            "tone?: boolean",
            "<p>{label}</p>",
            "",
            omit,
            true,
        ),
        (
            "type Props =",
            "tone?: boolean",
            "tone ? <b>{label}</b> : <p>{label}</p>",
            "",
            omit,
            true,
        ),
        (
            "interface Props",
            "tone: boolean",
            "<p>{label}</p>",
            "",
            omit,
            false,
        ),
        (
            "interface Props",
            "tone?: boolean",
            "<p>{label}</p>",
            "interface Props { extra?: string }",
            omit,
            false,
        ),
        (
            "export type Props =",
            "tone?: boolean",
            "<p>{label}</p>",
            "",
            omit,
            false,
        ),
        (
            "type Props =",
            "tone?: boolean",
            "<p>{label}</p>",
            "export const keys: Record<keyof Props, string> = {label: 'label'};",
            omit,
            false,
        ),
        (
            "interface Props",
            "tone?: boolean",
            "<p>{label}</p>",
            "",
            "<Panel {...{label:'a', tone:1}} />",
            false,
        ),
        (
            "type Props =",
            "tone?: boolean",
            "<p>{label}</p>",
            "",
            "<Panel label='a' tone='bad' />",
            false,
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let panel = |added: &str, binding: &str, body: &str| {
            format!(
                "{declaration} {{label: string; {added}}}\n{observer}\nexport function Panel({{ label{binding} }}: Props) {{ return {body}; }}\n"
            )
        };
        repo.write("package.json", r#"{"dependencies":{"react":"19.0.0"}}"#);
        repo.write("panel.tsx", panel("", "", "<p>{label}</p>"));
        repo.write(
            "home.tsx",
            format!("import {{Panel}} from './panel';\nexport const home = {caller};\n"),
        );
        repo.commit_all("base");
        repo.write("panel.tsx", panel(member, ", tone = false", body));
        repo.stage_all();
        for staged in [false, true] {
            if staged {
                repo.write("panel.tsx", "export {};");
            }
            let (output, result) = run_review(&repo, staged);
            let modified = result["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["n"] == "Panel");
            if compatible(&result) != safe
                || (!safe && declaration != "export type Props =" && !modified)
                || output.status.success() != safe
            {
                failures.push(format!(
                    "{declaration} / {member} / {observer} / {caller} / staged={staged}: {result}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
