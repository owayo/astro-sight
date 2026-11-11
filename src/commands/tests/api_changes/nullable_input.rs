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
