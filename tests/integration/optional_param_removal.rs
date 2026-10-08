use super::support::{TestRepo, cargo_bin};

#[test]
fn trailing_optional_removal_rejects_dynamic_execution_in_origin() {
    for execution in [
        "spacing(); eval('spacing(2)');",
        "spacing(); (0, eval)('spacing(2)');",
        "spacing(); new Function('spacing(2)')();",
        "spacing(); const evaluate = eval; evaluate('spacing(2)');",
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            format!(
                "export function spacing(scale = 1): number {{ return scale; }}\n{execution}\n"
            ),
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            format!("export function spacing(): number {{ return 1; }}\n{execution}\n"),
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{execution}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{json}");
    }
}

#[test]
fn trailing_optional_addition_keeps_existing_policy() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "spacing.ts",
        "export function spacing(): number { return 1; }\n",
    );
    repo.write(
        "use.ts",
        "import { spacing } from './spacing';\nspacing();\n",
    );
    repo.commit_all("initial");
    repo.write(
        "spacing.ts",
        "export function spacing(scale = 1): number { return scale; }\n",
    );
    let (output, json) = optional_removal_hook(&repo);
    assert!(output.status.success(), "{json}");
    assert_eq!(
        json["api"]["mod_compat"][0]["reason"], "trailing_optional_params",
        "{json}"
    );
}

fn optional_removal_hook(repo: &TestRepo) -> (std::process::Output, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
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

#[test]
fn trailing_optional_removal_spacing_is_repository_local_call_compatible() {
    for extension in ["ts", "tsx"] {
        let repo = TestRepo::new();
        repo.init_git();
        let file = format!("spacing.{extension}");
        repo.write(
            &file,
            "export function spacing(scale = 1): { gap: number } { return { gap: 12 * scale }; }\n",
        );
        repo.write(format!("use.{extension}"), "import { spacing } from './spacing';\nexport function gapOf(): number { return spacing().gap; }\n");
        repo.commit_all("initial");
        repo.write(
            &file,
            "export function spacing(): { gap: number } { return { gap: 12 }; }\n",
        );
        let api = repo.run_json("review", &["--git"]);
        assert_eq!(
            api["api_changes"]["compatible_modified"][0]["reason"],
            "trailing_optional_params_removed",
            "{api}"
        );
        let (output, json) = optional_removal_hook(&repo);
        assert!(output.status.success(), "{json}");
        assert_eq!(
            json["api"]["mod_compat"][0]["reason"], "trailing_optional_params_removed",
            "{json}"
        );
        assert!(json["impacts"].is_null(), "{json}");
        assert!(
            json["blocking_categories"].as_array().unwrap().is_empty(),
            "{json}"
        );
        assert!(
            json["impact_info"].as_array().unwrap().iter().any(|group| {
                group["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|reference| reference["ln"] == 1)
            }),
            "actual call must remain informational: {json}"
        );
    }
}

#[test]
fn trailing_optional_removal_keeps_remaining_parameters_and_comment_arguments() {
    for extension in ["ts", "tsx"] {
        for optional in ["scale = 1", "scale?: number"] {
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("spacing.{extension}");
            repo.write(&file, format!("export function spacing(size: number, {optional}): number {{ return size; }}\nfunction ownCall(): number {{ return spacing(2); }}\n"));
            repo.write(format!("use.{extension}"), "import { spacing } from './spacing';\nexport function read(): number { return spacing(/* ordinary comment */ 2, /* tail */); }\n");
            repo.commit_all("initial");
            repo.write(&file, "export function spacing(size: number): number { return size; }\nfunction ownCall(): number { return spacing(2); }\n");
            let (output, json) = optional_removal_hook(&repo);
            assert!(output.status.success(), "{json}");
            assert_eq!(
                json["api"]["mod_compat"][0]["reason"], "trailing_optional_params_removed",
                "{json}"
            );
        }
    }
}

#[test]
fn trailing_optional_removal_rejects_unproven_uses() {
    for extension in ["ts", "tsx"] {
        for usage in [
            "spacing(2);",
            "spac\\u0069ng(2); spacing();",
            "spacing(undefined);",
            "spacing(...values);",
            "const alias = spacing; alias();",
            "consume(spacing);",
            "type Args = Parameters<typeof spacing>; spacing();",
            "const registry = { spacing }; spacing();",
            "spacing.call(null);",
            "new spacing();",
            "spacing?.();",
            "(spacing)();",
            "spacing<string>();",
            "spacing(); function shadow(spacing: () => number) { return spacing(); }",
            "const view = <spacing />; spacing();",
        ] {
            if extension == "ts" && usage.contains("<spacing") {
                continue;
            }
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("spacing.{extension}");
            repo.write(
                &file,
                "export function spacing(scale = 1): number { return scale; }\n",
            );
            repo.write(
                format!("use.{extension}"),
                format!("import {{ spacing }} from './spacing';\n{usage}\n"),
            );
            repo.commit_all("initial");
            repo.write(&file, "export function spacing(): number { return 1; }\n");
            let (output, json) = optional_removal_hook(&repo);
            assert_eq!(output.status.code(), Some(1), "{usage}: {json}");
            assert!(json["api"]["mod_compat"].is_null(), "{usage}: {json}");
        }
    }
}

#[test]
fn trailing_optional_removal_rejects_contract_changes() {
    for (before, after) in [
        ("(scale: number): number", "(): number"),
        ("(scale = 1)", "()"),
        ("(scale = 1): number", "(): string"),
        (
            "(size: number, scale = 1): number",
            "(size: string): number",
        ),
        ("(size: number, scale = 1): 'a b'", "(size: number): 'a  b'"),
        (
            "(label: 'a b', scale = 1): number",
            "(label: 'a  b'): number",
        ),
        ("<T>(scale = 1): number", "<T>(): number"),
        (
            "(this: object, scale = 1): number",
            "(this: object): number",
        ),
        ("(scale = 1, ...values: number[]): number", "(): number"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            format!("export function spacing{before} {{ return 1; }}\n"),
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            format!("export function spacing{after} {{ return 1; }}\n"),
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{before} -> {after}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{json}");
    }
}

#[test]
fn trailing_optional_removal_rejects_hidden_module_transport() {
    for transport in [
        "eval('spacing(2)');",
        "ev\\u0061l('spacing(2)');",
        "requ\\u0069re('./spacing');",
        "Function('spacing(2)')();",
        "new Function('spacing(2)')();",
        "const evaluate = eval; evaluate('spacing(2)');",
        "const construct = Function; construct('spacing(2)')();",
        "globalThis['eval']('spacing(2)');",
        "export * from './spacing';",
        "export { other as forwarded } from './spacing';",
        "import * as helpers from './spacing';",
        "import { spacing as local } from './spacing';",
        "import { 'spacing' as local } from './spacing';",
        "import type { spacing } from './spacing';",
        "import('./spacing');",
        "import(`./spacing`);",
        "import(path);",
        "require('./spacing');",
        "require(path);",
        "const load = require; load('./spacing');",
        "consume(require);",
        "import.meta.glob('./*.ts');",
        "type Exported = typeof import('./spacing');",
        "declare module './spacing' { export function extra(): void; }",
        "import { spacing as local } from '@paths/spacing';",
        "import { spacing as local } from './spac\\u0069ng';",
        "import local = require('./spacing');",
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            "export function spacing(scale = 1): number { return scale; }\n",
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.write("transport.ts", transport);
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            "export function spacing(): number { return 1; }\n",
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{transport}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{transport}: {json}");
    }
}

#[test]
fn trailing_optional_removal_rejects_incomplete_inventory() {
    for (path, source) in [
        ("spacing.tsx", "export const other = 1;"),
        ("broken.ts", "const broken = ;"),
        ("component.vue", "<script>load('./spacing')</script>"),
        ("foreign.py", "spacing()"),
        (
            "generated.ts",
            "// @generated\nimport * as helpers from './spacing';",
        ),
        (
            "other.ts",
            "function spacing(): number { return 1; }\nspacing();",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            "export function spacing(scale = 1): number { return scale; }\n",
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.write(path, source);
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            "export function spacing(): number { return 1; }\n",
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{path}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{path}: {json}");
    }
}

#[test]
fn trailing_optional_removal_preserves_other_blocking_origins() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("spacing.ts", "export function spacing(scale = 1): number { return scale; }\nexport function other(): number { return 1; }\n");
    repo.write(
        "second.ts",
        "export function alternate(): number { return 1; }\n",
    );
    repo.write("use.ts", "import { spacing, other } from './spacing';\nimport { alternate } from './second';\nspacing(); other(); alternate();\n");
    repo.commit_all("initial");
    repo.write("spacing.ts", "export function spacing(): number { return 1; }\nexport function other(value: number): number { return value; }\n");
    repo.write(
        "second.ts",
        "export function alternate(value: number): number { return value; }\n",
    );
    let (output, json) = optional_removal_hook(&repo);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert_eq!(json["api"]["mod_compat"][0]["n"], "spacing", "{json}");
    let groups = json["impacts"].as_array().unwrap();
    assert!(
        groups.iter().any(|group| group["src"] == "spacing.ts"
            && group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol == "other")),
        "{json}"
    );
    assert!(
        groups.iter().any(|group| group["src"] == "second.ts"
            && group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol == "alternate")),
        "{json}"
    );
    assert!(
        groups.iter().all(|group| !group["syms"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol == "spacing")),
        "{json}"
    );
}
