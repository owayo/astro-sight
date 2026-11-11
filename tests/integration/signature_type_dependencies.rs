//! 公開宣言が同じファイルの非公開型に依存する契約変更。

use super::support::{TestRepo, cargo_bin};

#[test]
fn rust_opaque_return_contracts_block_worktree_and_index_reviews() {
    for staged in [false, true] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"opaque_demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        repo.write("src/lib.rs", "mod hidden;\npub fn produce() -> impl Copy { hidden::make() }\npub async fn future() { hidden::wait().await; }\npub fn ordinary() -> u8 { 0 }\n");
        let before = "type Hidden = ();\npub fn make() -> Hidden { Hidden::default() }\npub async fn wait() { let held = (); std::future::ready(()).await; drop(held); }\n";
        repo.write("src/hidden.rs", before);
        repo.commit_all("base");
        repo.write(
            "src/hidden.rs",
            before
                .replace(
                    "type Hidden = ();",
                    "type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;",
                )
                .replace("let held = ();", "let held = std::rc::Rc::new(());"),
        );
        if staged {
            repo.stage_all();
        }
        let mut cmd = cargo_bin();
        cmd.args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root());
        if staged {
            cmd.arg("--staged");
        }
        let output = cmd.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "staged={staged}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stderr)
            .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
        assert!(lists(&json["api"]["mod"], "produce"), "{json}");
        assert!(lists(&json["api"]["mod"], "future"), "{json}");
        assert!(!lists(&json["api"]["mod"], "ordinary"), "{json}");
        assert!(!lists(&json["api"]["mod_closed"], "produce"), "{json}");
        assert!(
            json["blocking_categories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|category| category == "api.mod"),
            "{json}"
        );
    }
}

fn lists(items: &serde_json::Value, name: &str) -> bool {
    items
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["n"] == name)
}

#[test]
fn public_signature_type_dependencies_report_changed_contracts() {
    for (before, after, call, changed, compatible) in [
        (
            "type Label = string;\nexport function format(value: Label): string { return String(value); }\n",
            "type Label = number;\nexport function format(value: Label): string { return String(value); }\n",
            "format('open')",
            true,
            false,
        ),
        (
            "interface Options { size: string; }\nexport function format(options: Options): string { return String(options.size); }\n",
            "interface Options { size: number; }\nexport function format(options: Options): string { return String(options.size); }\n",
            "format({ size: 'open' })",
            true,
            false,
        ),
        (
            "type Label = string;\nexport function format(value: Label, legacy?: boolean): string { return String(value); }\n",
            "type Label = number;\nexport function format(value: Label): string { return String(value); }\n",
            "format('open')",
            true,
            false,
        ),
        (
            "type Label = string;\nexport function format(value: Label): string { return String(value); }\n",
            "type Label = number;\nexport function format(value: Label, suffix?: string): string { return String(value); }\n",
            "format('open')",
            true,
            false,
        ),
        (
            "type Label = string;\nexport function format({ label }: { label: Label }): string { return String(label); }\n",
            "type Label = number;\nexport function format({ label }: { label: Label; suffix?: string }): string { return String(label); }\n",
            "format({ label: 'open' })",
            true,
            false,
        ),
        (
            "type Label = string;\nexport function format(value: Label, legacy?: boolean): string { return String(value); }\n",
            "type Label = string;\nexport function format(value: Label): string { return String(value); }\n",
            "format('open')",
            false,
            true,
        ),
        (
            "type Label = string;\nexport function format(value: Label): string { return String(value); }\n",
            "type Label = /* unchanged */ string ;\nexport function format(value: Label): string { return String(value); }\n",
            "format('open')",
            false,
            false,
        ),
        (
            "type Label = string;\nexport function format(value: string): string { return value; }\n",
            "type Label = number;\nexport function format(value: string, extra?: Label): string { return value; }\n",
            "format('open')",
            false,
            true,
        ),
        (
            "type Label = string;\nexport function format(value: string, legacy?: Label): string { return value; }\n",
            "type Label = number;\nexport function format(value: string): string { return value; }\n",
            "format('open')",
            false,
            true,
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("format.ts", before);
        repo.write(
            "app.ts",
            format!("import {{ format }} from './format';\nconsole.log({call});\n"),
        );
        repo.commit_all("initial");
        repo.write("format.ts", after);
        for staged in [false, true] {
            if staged {
                repo.git(["add", "."]);
                repo.write("format.ts", before);
            }
            let output = cargo_bin()
                .args(["review", "--git", "--hook", "--dir"])
                .arg(repo.root())
                .args(if staged { vec!["--staged"] } else { vec![] })
                .output()
                .unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&output.stderr).unwrap_or_default();
            assert_eq!(
                output.status.code(),
                Some(i32::from(changed)),
                "{after}, staged={staged}: {json}"
            );
            assert_eq!(
                lists(&json["api"]["mod"], "format"),
                changed,
                "{after}: {json}"
            );
            assert_eq!(
                lists(&json["api"]["mod_compat"], "format"),
                compatible,
                "{after}: {json}"
            );
            assert!(
                !lists(&json["api"]["mod_closed"], "format"),
                "{after}: {json}"
            );
            if changed {
                assert!(
                    json["impacts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|group| group["refs"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|reference| reference["p"] == "app.ts" && reference["ln"] == 1)),
                    "{json}"
                );
            }
        }
    }
}

#[test]
fn public_signature_type_dependencies_cover_return_class_and_variable_contracts() {
    for (declaration, name, call) in [
        (
            "export function format(): Label { return 'open' as any; }",
            "format",
            "format()",
        ),
        (
            "export class View { constructor(public value: Label) {} }",
            "View",
            "new View('open')",
        ),
        (
            "export const format: (value: Label) => string = value => String(value);",
            "format",
            "format('open')",
        ),
        (
            "export const format = (value: Label): string => String(value);",
            "format",
            "format('open')",
        ),
        (
            "export const format = (((value: Label): string => String(value)));",
            "format",
            "format('open')",
        ),
        (
            "class Hidden { value!: Label } export function format(value: Hidden): string { return String(value.value); }",
            "format",
            "format({ value: 'open' })",
        ),
        (
            "class Base { value!: Label } class Hidden extends Base {} export function format(value: Hidden): string { return String(value.value); }",
            "format",
            "format({ value: 'open' })",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let source = format!("type Label = string;\n{declaration}\n");
        repo.write("format.ts", &source);
        repo.write(
            "app.ts",
            format!("import {{ {name} }} from './format';\nconsole.log({call});\n"),
        );
        repo.commit_all("initial");
        repo.write(
            "format.ts",
            source.replace("Label = string", "Label = number"),
        );
        let review = repo.run_json("review", &["--git"]);
        assert!(
            review["api_changes"]["modified"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["name"] == name),
            "{review}"
        );
        assert!(
            review["impact"]["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|impact| impact["affected_symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|symbol| symbol["name"] == name)
                    && impact["impacted_callers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|caller| caller["path"] == "app.ts" && caller["line"] == 1)),
            "{review}"
        );
    }
}

#[test]
fn public_signature_type_dependencies_keep_unknown_imports_separate_from_changes() {
    for (specifier, package, compatible) in [("./other", false, false), ("pkg", true, true)] {
        let repo = TestRepo::new();
        repo.init_git();
        let source = format!(
            "import {{ type External }} from '{specifier}';\ntype Unused = string;\nexport function format(value: External): string {{ return String(value); }}\n"
        );
        repo.write("format.ts", &source);
        repo.write("other.ts", "export interface External { value: string }\n");
        repo.write(
            "app.ts",
            "import { format } from './format';\nconsole.log(format({ value: 'open' }));\n",
        );
        if package {
            repo.write("package.json", r#"{"dependencies":{"pkg":"^1.0.0"}}"#);
        }
        repo.commit_all("initial");
        repo.write(
            "format.ts",
            source.replace("Unused = string", "Unused = number"),
        );
        let output = cargo_bin()
            .args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap_or_default();
        assert!(!lists(&json["api"]["mod"], "format"), "{json}");
        repo.write(
            "format.ts",
            source.replace("value: External)", "value: External, extra?: boolean)"),
        );
        let output = cargo_bin()
            .args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap_or_default();
        assert_eq!(
            output.status.code(),
            Some(i32::from(!compatible)),
            "{specifier}: {json}"
        );
        assert_eq!(
            lists(&json["api"]["mod_compat"], "format"),
            compatible,
            "{json}"
        );
        assert_eq!(lists(&json["api"]["mod"], "format"), !compatible, "{json}");
    }
}

#[test]
fn public_signature_type_dependency_impacts_respect_caller_exclusions() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("ignored");
    repo.write("format.ts", "type Label = string; export function format(value: Label): string { return String(value); }\n");
    repo.write(
        "ignored/app.ts",
        "import { format } from '../format';\nconsole.log(format('open'));\n",
    );
    repo.commit_all("initial");
    repo.write("format.ts", "type Label = number; export function format(value: Label): string { return String(value); }\n");
    let output = cargo_bin()
        .current_dir(repo.root())
        .args(["review", "--git", "--dir", "."])
        .output()
        .unwrap();
    assert!(output.status.success());
    let relative: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        relative["impact"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|impact| impact["impacted_callers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|caller| caller["path"] == "ignored/app.ts")),
        "{relative}"
    );
    for extra in [
        ["--exclude-dir", "ignored"],
        ["--exclude-glob", "ignored/**"],
    ] {
        let review = repo.run_json("review", &["--git", extra[0], extra[1]]);
        assert!(
            review["impact"]["changes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|impact| impact["impacted_callers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .all(|caller| caller["path"] != "ignored/app.ts")),
            "{review}"
        );
    }
}

#[test]
fn public_signature_type_dependency_impacts_use_language_and_import_ownership() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("format.ts", "type Label = string;\nexport function format(value: Label): string { return String(value); }\n");
    repo.write("barrel.ts", "export { format } from './format';\n");
    repo.write(
        "app.ts",
        "import { format } from './barrel';\nconsole.log(format('open'));\n",
    );
    repo.write(
        "direct.ts",
        "import { format } from './format';\nconsole.log(format('open'));\n",
    );
    repo.write(
        "other.ts",
        "export function format(value: string): string { return value; }\n",
    );
    repo.write(
        "other_app.ts",
        "import { format } from './other';\nconsole.log(format('open'));\n",
    );
    repo.write(
        "unrelated.rs",
        "fn format(value: &str) {}\nfn main() { format(\"open\"); }\n",
    );
    repo.commit_all("initial");
    repo.write("format.ts", "type Label = number;\nexport function format(value: Label): string { return String(value); }\n");
    let review = repo.run_json("review", &["--git"]);
    let callers: Vec<_> = review["impact"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|impact| impact["path"] == "format.ts")
        .flat_map(|impact| impact["impacted_callers"].as_array().into_iter().flatten())
        .map(|caller| caller["path"].as_str().unwrap())
        .collect();
    assert!(callers.contains(&"direct.ts"), "{review}");
    assert!(callers.contains(&"app.ts"), "{review}");
    assert!(!callers.contains(&"other_app.ts"), "{review}");
    assert!(!callers.contains(&"unrelated.rs"), "{review}");
}
