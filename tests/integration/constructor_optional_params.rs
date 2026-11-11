use super::support::{TestRepo, cargo_bin};

fn constructor_repo(extension: &str, old: &str, new: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    let file = format!("errors.{extension}");
    repo.write(&file, old);
    repo.write(
        format!("main.{extension}"),
        "import { FetchError } from './errors';\nexport function fail(): never { throw new FetchError('timeout'); }\n",
    );
    repo.commit_all("initial");
    repo.write(&file, new);
    repo
}

fn hook(repo: &TestRepo, staged: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = cargo_bin();
    command.args(["review", "--git", "--hook", "--dir"]);
    command.arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let json = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "invalid hook output: {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, json)
}

#[test]
fn constructor_trailing_optional_and_default_parameters_are_compatible() {
    let old = "export class FetchError extends Error { constructor(message: string) { super(message); } }\n";
    for extension in ["ts", "tsx"] {
        for tail in [
            ", retryable = false",
            ", retryable?: boolean",
            ", retryable = false, code?: number",
        ] {
            let new = old.replace("message: string)", &format!("message: string{tail})"));
            let repo = constructor_repo(extension, old, &new);
            let review = repo.run_json("review", &["--git"]);
            let api = &review["api_changes"];
            assert_eq!(
                api["compatible_modified"][0]["name"], "FetchError",
                "{review}"
            );
            assert_eq!(
                api["compatible_modified"][0]["reason"], "trailing_optional_params",
                "{review}"
            );
            assert!(api["modified"].as_array().unwrap().is_empty(), "{review}");
            for staged in [false, true] {
                if staged {
                    repo.stage_all();
                }
                let (output, json) = hook(&repo, staged);
                assert!(output.status.success(), "{tail}: {json}");
                assert_eq!(json["api"]["mod_compat"][0]["n"], "FetchError", "{json}");
                assert!(json["impacts"].is_null(), "{json}");
                assert!(
                    json["blocking_categories"].as_array().unwrap().is_empty(),
                    "{json}"
                );
            }
        }
    }
}

#[test]
fn constructor_non_optional_or_other_contract_changes_stay_blocking() {
    let old = "export class FetchError extends Error { code: number = 1; constructor(message: string) { super(message); } }\n";
    for new in [
        old.replace("message: string)", "message: string, retryable: boolean)"),
        old.replace("message: string)", "message: number, retryable = false)"),
        old.replace(
            "constructor(message: string)",
            "protected constructor(message: string, retryable = false)",
        ),
        old.replace("extends Error", "extends TypeError")
            .replace("message: string)", "message: string, retryable = false)"),
        old.replace("code: number", "code: string")
            .replace("message: string)", "message: string, retryable = false)"),
    ] {
        let repo = constructor_repo("ts", old, &new);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{new}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{new}: {json}");
        assert!(
            json["blocking_categories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "api.mod"),
            "{json}"
        );
    }
}

#[test]
fn constructor_parameter_properties_and_overloads_are_not_proven_compatible() {
    let old = "export class FetchError extends Error { constructor(message: string) { super(message); } }\n";
    for new in [
        old.replace(
            "message: string)",
            "message: string, readonly retryable = false)",
        ),
        old.replace(
            "message: string)",
            "message: string, public retryable = false)",
        ),
        old.replace(
            "message: string)",
            "message: string, private retryable = false)",
        ),
        old.replace(
            "constructor(message: string)",
            "constructor(message: string); constructor(message: string, retryable = false)",
        ),
    ] {
        let repo = constructor_repo("ts", old, &new);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{new}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{new}: {json}");
    }
}

#[test]
fn constructor_absent_implementation_and_duplicate_classes_remain_unverified() {
    for (old, new) in [
        (
            "export class FetchError extends Error {}\n",
            "export class FetchError extends Error { constructor(message: string, retryable = false) { super(message); } }\n",
        ),
        (
            "export declare class FetchError extends Error { constructor(message: string); }\n",
            "export declare class FetchError extends Error { constructor(message: string, retryable?: boolean); }\n",
        ),
        (
            "export class FetchError extends Error { constructor(message: string) { super(message); } }\nclass FetchError {}\n",
            "export class FetchError extends Error { constructor(message: string, retryable = false) { super(message); } }\nclass FetchError {}\n",
        ),
    ] {
        let repo = constructor_repo("ts", old, new);
        let review = repo.run_json("review", &["--git"]);
        assert!(
            review["api_changes"]["compatible_modified"].is_null(),
            "{review}"
        );
    }
}

#[test]
fn constructor_optional_parameters_keep_changed_local_type_dependencies_blocking() {
    for (base, heritage, member, body) in [
        (
            "class Base { detail!: Detail; }",
            " extends Base",
            "",
            "super();",
        ),
        ("", " extends Error", "detail!: Detail;", "super(message);"),
        (
            "",
            " extends Error",
            "describe(): Detail { return null as any; }",
            "super(message);",
        ),
    ] {
        let old = format!(
            "type Detail = string;\n{base}\nexport class FetchError{heritage} {{ {member} constructor(message: string) {{ {body} }} }}\n"
        );
        let new = old
            .replace("type Detail = string", "type Detail = number")
            .replace("message: string)", "message: string, retryable = false)");
        let repo = constructor_repo("ts", &old, &new);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{new}: {json}");
        assert!(
            json["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["n"] == "FetchError"),
            "{new}: {json}"
        );
        assert!(json["api"]["mod_compat"].is_null(), "{new}: {json}");
    }
}

#[test]
fn constructor_optional_parameters_keep_changed_external_dependencies_blocking() {
    for (prelude, member, path, old_dependency, new_dependency) in [
        (
            "import type { Detail } from './types';",
            "detail!: Detail;",
            "types.ts",
            "export type Detail = string;\n",
            "export type Detail = number;\n",
        ),
        (
            "",
            "",
            "globals.d.ts",
            "export {}; declare global { interface Error { detail: string; } }\n",
            "export {}; declare global { interface Error { detail: number; } }\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let old = format!(
            "{prelude}\nexport class FetchError extends Error {{ {member} constructor(message: string) {{ super(message); }} }}\n"
        );
        repo.write("errors.ts", &old);
        repo.write(path, old_dependency);
        repo.write(
            "main.ts",
            "import { FetchError } from './errors'; export const error = new FetchError('timeout');\n",
        );
        repo.commit_all("initial");
        repo.write(
            "errors.ts",
            old.replace("message: string)", "message: string, retryable = false)"),
        );
        repo.write(path, new_dependency);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{path}: {json}");
        assert!(
            json["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["n"] == "FetchError"),
            "{path}: {json}"
        );
        assert!(json["api"]["mod_compat"].is_null(), "{path}: {json}");
    }
}
