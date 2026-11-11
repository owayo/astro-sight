use super::support::{TestRepo, cargo_bin};

const BEFORE: &str = "export interface Parcel<T = unknown> { valid: boolean; content?: T; }\n";
const AFTER: &str = "export interface Parcel<T = unknown, M = unknown> { valid: boolean; content?: T; metadata?: M; }\n";
const READER: &str = "import type { Parcel } from './shapes';\nexport function consume(value: Parcel<number>): boolean { return value.valid; }\n";

fn repo(before: &str, after: &str, reader: &str, extra: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("shapes.ts", before);
    repo.write("reader.ts", reader);
    for &(path, source) in extra {
        repo.write(path, source);
    }
    repo.commit_all("base");
    repo.write("shapes.ts", after);
    repo
}

fn hook(repo: &TestRepo, extra: &[&str]) -> (std::process::Output, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .args(extra)
        .output()
        .unwrap();
    let result = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    (output, result)
}

fn has(items: &serde_json::Value, name: &str) -> bool {
    items
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["n"] == name)
}

fn assert_compatible(repo: &TestRepo, extra: &[&str]) -> serde_json::Value {
    let (output, result) = hook(repo, extra);
    assert!(output.status.success(), "{result}");
    assert!(
        result["api"]["mod_compat"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| item["n"] == "Parcel" && item["reason"] == "trailing_default_type_params"),
        "{result}"
    );
    assert!(!has(&result["api"]["mod"], "Parcel"), "{result}");
    assert!(result["impacts"].is_null(), "{result}");
    result
}

fn assert_blocking(repo: &TestRepo) {
    let (output, result) = hook(repo, &[]);
    assert_eq!(output.status.code(), Some(1), "{result}");
    assert!(has(&result["api"]["mod"], "Parcel"), "{result}");
    assert!(!has(&result["api"]["mod_compat"], "Parcel"), "{result}");
}

#[test]
fn trailing_default_type_params_supports_old_arity_readers() {
    for after in [
        "export interface Parcel<T = unknown, M = unknown> { valid: boolean; content?: T; }\n",
        AFTER,
        "export interface Parcel<T = unknown, M = unknown, N = number> { valid: boolean; content?: T; metadata?: M; }\n",
    ] {
        let result = assert_compatible(&repo(BEFORE, after, READER, &[]), &[]);
        assert!(result["impact_info"].is_array(), "{result}");
    }
    assert_compatible(
        &repo(
            BEFORE,
            AFTER,
            &READER.replace("Parcel<number>", "Parcel"),
            &[],
        ),
        &[],
    );
    let alias_before = BEFORE
        .replace("interface Parcel", "type Parcel")
        .replace(" {", " = {");
    let alias_after = AFTER
        .replace("interface Parcel", "type Parcel")
        .replace(" {", " = {");
    assert_compatible(&repo(&alias_before, &alias_after, READER, &[]), &[]);
}

#[test]
fn trailing_default_type_params_preserves_declaration_controls() {
    for after in [
        "export interface Parcel<T = unknown, M> { valid: boolean; content?: T; }",
        "export interface Parcel<M = unknown, T = unknown> { valid: boolean; content?: T; }",
        "export interface Parcel<T extends object = unknown, M = unknown> { valid: boolean; content?: T; }",
        "export interface Parcel<T = unknown, M = unknown> { valid: string; content?: T; }",
        "export interface Parcel<T = unknown, M extends string = number> { valid: boolean; content?: T; }",
        "export interface Parcel<T = unknown, M = Missing> { valid: boolean; content?: T; }",
        "export interface Parcel<T = string, M = unknown> { valid: boolean; content?: T; }",
        "export interface Parcel<T = unknown, M = string> { valid: boolean; content?: T; metadata?: M; }",
    ] {
        assert_blocking(&repo(BEFORE, after, READER, &[]));
    }
    let before =
        "type M = string; export interface Parcel<T = unknown> { valid: boolean; content?: M; }";
    let after = "type M = string; export interface Parcel<T = unknown, M = unknown> { valid: boolean; content?: M; }";
    assert_blocking(&repo(before, after, READER, &[]));
    let before = "type M = string; export interface Parcel<T = M> { valid: boolean; content?: T; }";
    let after = "type M = string; export interface Parcel<T = M, M = unknown> { valid: boolean; content?: T; }";
    assert_blocking(&repo(before, after, READER, &[]));
    let before = "export interface Parcel<T = unknown> { valid: boolean; content?: Parcel<T>; }";
    let after = "export interface Parcel<T = unknown, M = unknown> { valid: boolean; content?: Parcel<T>; metadata?: M; }";
    assert_blocking(&repo(before, after, READER, &[]));
}

#[test]
fn trailing_default_type_params_preserves_sensitive_type_uses() {
    for extra in [
        "type Keys = keyof Parcel<number>;",
        "type All = Required<Parcel<number>>;",
        "type Copy = { [P in keyof Parcel<number>]: Parcel<number>[P] };",
        "interface Derived extends Parcel<number> {}",
        "class Derived implements Parcel<number> { valid = true; private metadata = 1; }",
        "export { Parcel };",
        "const value: Parcel<number> = { valid: true };",
    ] {
        assert_blocking(&repo(BEFORE, AFTER, &format!("{READER}\n{extra}\n"), &[]));
    }
    for body in [
        "return external(value);",
        "value.valid = false; return value.valid;",
        "const copy = value; return copy.valid;",
    ] {
        let reader = format!(
            "import type {{ Parcel }} from './shapes'; export function consume(value: Parcel<number>): boolean {{ {body} }}"
        );
        assert_blocking(&repo(BEFORE, AFTER, &reader, &[]));
    }
    for extra in [
        "import type { Parcel as Outside } from 'external';",
        "import * as shapes from './shapes';",
        "declare module './shapes' { interface Parcel<T = unknown> { extended: string; } }",
    ] {
        assert_blocking(&repo(BEFORE, AFTER, READER, &[("extra.ts", extra)]));
    }
}

#[test]
fn trailing_default_type_params_audits_ignored_sensitive_uses() {
    let repo = repo(BEFORE, AFTER, READER, &[(".gitignore", "ignored.ts\n")]);
    repo.write(
        "ignored.ts",
        "import type { Parcel } from './shapes'; type Keys = keyof Parcel<number>;",
    );
    assert_blocking(&repo);
}

#[test]
fn trailing_default_type_params_keeps_structural_callers_blocking() {
    let calls = "class Secret { valid = true; private metadata = 1; } consume(new Secret());";
    assert_blocking(&repo(BEFORE, AFTER, &format!("{READER}\n{calls}"), &[]));
    for source in [
        "import { consume } from './reader'; class Secret { valid = true; private metadata = 1; } consume(new Secret());",
        "import { consume as run } from './reader'; export { run };",
        "export * from './reader';",
    ] {
        assert_blocking(&repo(BEFORE, AFTER, READER, &[("caller.ts", source)]));
    }
    let repo = repo(BEFORE, AFTER, READER, &[(".gitignore", "ignored.ts\n")]);
    repo.write(
        "ignored.ts",
        "import { consume } from './reader'; consume(external());",
    );
    assert_blocking(&repo);
}

#[test]
fn trailing_default_type_params_staged_uses_the_index() {
    let repo = repo(BEFORE, AFTER, READER, &[]);
    repo.stage_all();
    repo.write(
        "shapes.ts",
        "export interface Parcel<T = unknown, M> { valid: string; content?: T; }",
    );
    assert_compatible(&repo, &["--staged"]);
}

#[test]
fn trailing_default_type_params_normal_json_is_informational() {
    let repo = repo(BEFORE, AFTER, READER, &[]);
    let result = repo.run_json("review", &["--git"]);
    assert!(
        result["api_changes"]["compatible_modified"]
            .as_array()
            .into_iter()
            .flatten()
            .any(
                |item| item["name"] == "Parcel" && item["reason"] == "trailing_default_type_params"
            ),
        "{result}"
    );
    assert!(
        result["api_changes"]["modified"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{result}"
    );
}
