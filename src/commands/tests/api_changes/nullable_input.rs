use crate::commands::tests::common::*;
use crate::models::review::{TypeChangeDirection, TypeChangePosition};

const BEFORE: &str = "export function word(text: string): string { return String(text); }\n";
const AFTER: &str = "export function word(text: string | null): string { return String(text); }\n";
const CALL: &str = "import { word } from './render';\nconsole.log(word('a'));\n";

fn changes(
    before: &str,
    after: &str,
    caller: &str,
    extra: &[(&str, &str)],
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo_for_test(dir.path());
    let files = [("render.ts", before), ("call.ts", caller)]
        .into_iter()
        .chain(extra.iter().copied())
        .collect::<Vec<_>>();
    git_commit_files(dir.path(), &files, "initial");
    std::fs::write(dir.path().join("render.ts"), after).unwrap();
    detect_api_changes_from_worktree(dir.path())
}

#[test]
fn nullable_input_direct_calls_and_inline_props_are_compatible() {
    for (before, after, caller, path) in [
        (BEFORE, AFTER, CALL, Vec::new()),
        (
            "export function word(props: { text: string }): string { return String(props.text); }\n",
            "export function word(props: { text: string | null }): string { return String(props.text); }\n",
            "import { word } from './render';\nword({ text: 'a' });\n",
            vec!["text"],
        ),
        (
            "export function word(text: 'a' | 'b'): string { return String(text); }\n",
            "export function word(text: null | 'b' | 'a'): string { return String(text); }\n",
            CALL,
            Vec::new(),
        ),
    ] {
        let api = changes(before, after, caller, &[]);
        assert!(api.modified.is_empty(), "{api:?}");
        assert!(
            api.compatible_modified
                .iter()
                .any(|c| c.reason == "nullable_input_widening"),
            "{api:?}"
        );
        let evidence = serde_json::to_value(&api.type_change_evidence).unwrap();
        assert_eq!(evidence[0]["usage"], "direct_calls_proven");
        assert_eq!(
            evidence[0]["changes"][0]["property_path"],
            serde_json::json!(path)
        );
        assert_eq!(evidence[0]["changes"][0]["direction"], "widening");
    }
}

#[test]
fn nullable_input_checks_expected_diagnostics_in_declarations_and_nested_arguments() {
    let before = "// @ts-expect-error null default\nexport function word(text: string = null): string { return String(text); }\n";
    let after = before.replace("text: string", "text: string | null");
    let api = changes(before, &after, CALL, &[]);
    assert!(api.modified.iter().any(|c| c.name == "word"), "{api:?}");

    let before = "export function word(options: { text: string }): string { return String(options.text); }\n";
    let after = before.replace("text: string", "text: string | null");
    let caller = "import { word } from './render';\nword({\n// @ts-expect-error null was rejected\ntext: null });\n";
    let api = changes(before, &after, caller, &[]);
    assert!(api.modified.iter().any(|c| c.name == "word"), "{api:?}");

    let other = "// Calling word with null used to fail.\n// @ts-expect-error number\nconst bad: string = 5;\n";
    let api = changes(BEFORE, AFTER, CALL, &[("other.ts", other)]);
    assert_eq!(api.compatible_modified[0].reason, "nullable_input_widening");
}

#[test]
fn nullable_type_directions_do_not_imply_compatibility() {
    for (before, after, position, direction) in [
        (
            AFTER,
            BEFORE,
            TypeChangePosition::Input,
            TypeChangeDirection::Narrowing,
        ),
        (
            BEFORE,
            "export function word(text: string): string | null { return String(text); }\n",
            TypeChangePosition::Return,
            TypeChangeDirection::Widening,
        ),
        (
            "export function word(text: string): string | null { return String(text); }\n",
            BEFORE,
            TypeChangePosition::Return,
            TypeChangeDirection::Narrowing,
        ),
    ] {
        let api = changes(before, after, CALL, &[]);
        assert!(api.modified.iter().any(|c| c.name == "word"), "{api:?}");
        assert!(api.compatible_modified.is_empty(), "{api:?}");
        assert_eq!(api.type_change_evidence[0].changes[0].position, position);
        assert_eq!(api.type_change_evidence[0].changes[0].direction, direction);
        assert_eq!(
            serde_json::to_value(&api.type_change_evidence).unwrap()[0]["usage"],
            "unproven"
        );
    }
}

#[test]
fn nullable_input_requires_closed_read_only_direct_use() {
    for caller in [
        "import { word } from './render';\nconst callback = word;\ncallback('a');\n",
        "import { word } from './render';\nregister(word);\nword('a');\n",
        "import { word } from './render';\nword = replacement;\nword('a');\n",
        "import { word } from './render';\ntype Input = Parameters<typeof word>[0];\nword('a');\n",
        "import { word } from './render';\ntype Props = ComponentProps<typeof word>;\nword('a');\n",
        "export { word } from './render';\n",
        "import { word } from './render';\nword(...args);\n",
    ] {
        let api = changes(BEFORE, AFTER, caller, &[]);
        assert!(
            api.modified.iter().any(|c| c.name == "word"),
            "{caller}: {api:?}"
        );
        assert!(api.compatible_modified.is_empty(), "{caller}: {api:?}");
    }
    for manifest in [
        r#"{"name":"public-package"}"#,
        r#"{"private":true,"exports":"./render.ts"}"#,
        r#"{"main":"./render.ts"}"#,
        r#"{"types":"./render.ts"}"#,
    ] {
        let api = changes(BEFORE, AFTER, CALL, &[("package.json", manifest)]);
        assert!(
            api.modified.iter().any(|c| c.name == "word"),
            "{manifest}: {api:?}"
        );
    }
}

#[test]
fn nullable_input_rejects_other_contract_changes_and_function_props() {
    let function_prop = changes(
        "export function word(props: { handler: (v: string) => void }): string { return ''; }\n",
        "export function word(props: { handler: (v: string | null) => void }): string { return ''; }\n",
        "import { word } from './render';\nword({ handler: (v: string) => console.log(v.length) });\n",
        &[],
    );
    assert!(
        function_prop.modified.iter().any(|c| c.name == "word"),
        "{function_prop:?}"
    );
    assert!(
        function_prop.compatible_modified.is_empty(),
        "{function_prop:?}"
    );
    for (before, after) in [
        (
            BEFORE,
            "export function word(text: string | null): string { return 'different'; }\n",
        ),
        (
            BEFORE,
            "export function word(text: string | null): string | null { return String(text); }\n",
        ),
        (
            "export function word(props: { handler: (v: string) => void }): string { return ''; }\n",
            "export function word(props: { handler: (v: string | null) => void }): string { return ''; }\n",
        ),
        (
            "type Text = (v: string) => void; export function word(text: Text): string { return ''; }\n",
            "type Text = (v: string) => void; export function word(text: Text | null): string { return ''; }\n",
        ),
        (
            "export function word(text: 'a'): string { return String(text); }\n",
            "export function word(text: 'a' | 'b'): string { return String(text); }\n",
        ),
        (
            "type Result = string; export function word(text: string): Result { return String(text); }\n",
            "type Result = number; export function word(text: string | null): Result { return String(text); }\n",
        ),
    ] {
        let api = changes(before, after, CALL, &[]);
        assert!(
            api.modified.iter().any(|c| c.name == "word"),
            "{after}: {api:?}"
        );
        assert!(api.compatible_modified.is_empty(), "{after}: {api:?}");
    }
}

#[test]
fn parameter_union_widening_retains_all_old_arms_and_type_bindings() {
    let before = "type A = 'a';\ntype B = 'b';\ntype C = 'c';\nexport function word(text: A | B) { return `${text}`; }\n";
    let after = before.replace(": A | B", ": (B | C) | A");
    let api = changes(before, &after, CALL, &[]);
    assert!(api.modified.is_empty(), "{api:?}");
    assert_eq!(
        api.compatible_modified[0].reason,
        "parameter_union_widening"
    );
    assert_eq!(api.type_change_evidence[0].changes[0].member, "C");

    for after in [
        before.replace(": A | B", ": A | C"),
        after.replace("type A = 'a'", "type A = 'other'"),
        after.replace("type C = 'c'", "type C = (value: number) => void"),
    ] {
        let api = changes(before, &after, CALL, &[]);
        assert!(api.modified.iter().any(|c| c.name == "word"), "{api:?}");
    }
}

#[test]
fn parameter_union_widening_keeps_expected_type_errors_blocking() {
    let before = "export function word(text: string) { return 'fixed'; }\n";
    let after = before.replace("text: string", "text: string | undefined");
    let config = [("tsconfig.json", r#"{"compilerOptions":{"strict":true}}"#)];
    let caller = "import { word } from './render';\n// @ts-expect-error\nword(undefined);\n";
    let api = changes(before, &after, caller, &config);
    assert!(api.modified.iter().any(|c| c.name == "word"), "{api:?}");
    let caller = "import { word } from './render';\nword('@ts-expect-error');\n";
    let api = changes(before, &after, caller, &config);
    assert!(api.modified.is_empty(), "{api:?}");
    assert_eq!(
        api.compatible_modified[0].reason,
        "parameter_union_widening"
    );
}

#[test]
fn parameter_union_widening_requires_unchanged_call_and_return_contracts() {
    for (before, after) in [
        ("text: string", "text: number | null"),
        ("text: string | null", "text: string"),
        ("text: string", "text?: string | undefined"),
        (
            "text: string, count?: number",
            "text: string | undefined, count: number",
        ),
        (
            "text: (x: string) => void",
            "text: (x: string | undefined) => void",
        ),
        (
            "text: string, suffix: string = text.trim()",
            "text: string | undefined, suffix: string = text.trim()",
        ),
    ] {
        let decl = |params: &str| format!("export function word({params}) {{ return 'fixed'; }}\n");
        let api = changes(&decl(before), &decl(after), CALL, &[]);
        assert!(
            api.modified.iter().any(|c| c.name == "word"),
            "{after}: {api:?}"
        );
    }
    for body in [
        "return text;",
        "return { text };",
        "return `${text.trim()}`;",
        "return `${text ?? Symbol()}`;",
        "return text !== null;",
    ] {
        let before = format!("export function word(text: string) {{ {body} }}\n");
        let after = before.replace("text: string", "text: string | null");
        let api = changes(&before, &after, CALL, &[]);
        assert!(
            api.modified.iter().any(|c| c.name == "word"),
            "{body}: {api:?}"
        );
    }
}

#[test]
fn parameter_union_widening_proves_unused_data_types_and_checks_contextual_types() {
    let objects = "type A = { a: number };\ninterface B { b: string }\ninterface C { c: boolean }\nexport function word(text: A | B) { return 1; }\n";
    let caller = "import { word } from './render';\nword({ a: 1 });\n";
    let api = changes(
        objects,
        &objects.replace(": A | B", ": A | B | C"),
        caller,
        &[],
    );
    assert!(api.modified.is_empty(), "{api:?}");
    assert_eq!(
        api.compatible_modified[0].reason,
        "parameter_union_widening"
    );
    for (before, after, caller) in [
        (
            "export function word(text: 'a'): 'a' { return `${text}`; }\n",
            "export function word(text: 'a' | 'b'): 'a' { return `${text}`; }\n",
            CALL,
        ),
        (
            "type A = { run: (x: string) => void }; type B = { run: (x: number) => void };\nexport function word(text: A) { return 1; }\n",
            "type A = { run: (x: string) => void }; type B = { run: (x: number) => void };\nexport function word(text: A | B) { return 1; }\n",
            "import { word } from './render';\nword({ run: x => x.trim() });\n",
        ),
    ] {
        let api = changes(before, after, caller, &[]);
        assert!(api.modified.iter().any(|c| c.name == "word"), "{api:?}");
    }
}
