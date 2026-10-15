use super::support::*;

const BEFORE: &str = "export function ControlPanel({ options }: { options?: { key: string } }) {\n  return <span>{options?.key}</span>;\n}\n";
const AFTER: &str = "export function ControlPanel({ options }: { options?: { key: string; category: string } }) {\n  return <span>{options?.key}</span>;\n}\n";

fn jsx_repo(before: &str, after: &str, caller: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.write("panel.tsx", before);
    repo.write("use.tsx", format!("import {{ ControlPanel }} from './panel';\nexport function Page() {{ return {caller}; }}\n"));
    repo.init_git();
    repo.commit_all("initial");
    repo.write("panel.tsx", after);
    repo
}

fn has_caller(
    context: &serde_json::Value,
    source: &str,
    bucket: &str,
    path: &str,
    line: u64,
) -> bool {
    context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| change["path"] == source)
        .any(|change| {
            change[bucket].as_array().is_some_and(|callers| {
                callers
                    .iter()
                    .any(|caller| caller["path"] == path && caller["line"] == line)
            })
        })
}

#[test]
fn impact_jsx_props_omitted_optional_nested_change_is_informational() {
    let repo = jsx_repo(BEFORE, BEFORE, "<ControlPanel />");
    repo.write("configured.tsx", "import { ControlPanel } from './panel';\nexport function Configured() { return <ControlPanel options={{ key: 'a' }} />; }\n");
    repo.commit_all("configured caller");
    repo.write("panel.tsx", AFTER);
    repo.write("configured.tsx", "import { ControlPanel } from './panel';\nexport function Configured() { return <ControlPanel options={{ key: 'a', category: 'primary' }} />; }\n");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        !has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
        "{context}"
    );
    assert!(
        has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
        "{context}"
    );
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        hook["api"]["mod"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["n"] == "ControlPanel"),
        "{hook}"
    );
    assert!(
        hook["impacts"].as_array().is_none_or(Vec::is_empty),
        "{hook}"
    );
}

#[test]
fn impact_jsx_props_supplied_unknown_and_mixed_callers_stay_blocking() {
    for caller in [
        "<ControlPanel options={{key:'a'}} />",
        "<ControlPanel options />",
        "<ControlPanel options={undefined}></ControlPanel>",
        "<ControlPanel {...props} />",
        "<ControlPanel<number> />",
        "<ControlPanel></Other>",
        "[<ControlPanel />, <ControlPanel options={{key:'a'}} />]",
    ] {
        let repo = jsx_repo(BEFORE, AFTER, caller);
        let context = repo.run_json("context", &["--git"]);
        assert!(
            has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
            "{caller}: {context}"
        );
        if !caller.starts_with('[') {
            assert!(
                !has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
                "{caller}: {context}"
            );
        }
    }
    for caller in ["<NS.ControlPanel />", "<NS:ControlPanel />"] {
        let repo = jsx_repo(BEFORE, AFTER, caller);
        repo.write(
            "use.tsx",
            format!(
                "import * as NS from './panel';\nexport function Page() {{ return {caller}; }}\n"
            ),
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            !has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
            "{context}"
        );
    }
}

#[test]
fn impact_jsx_props_all_changed_keys_and_implicit_children_are_checked() {
    let before = BEFORE.replace("options?:", "theme?: { key: string }; options?:");
    let after = AFTER.replace(
        "options?:",
        "theme?: { key: string; category: string }; options?:",
    );
    for (caller, blocking) in [
        ("<ControlPanel />", false),
        ("<ControlPanel theme={{key:'a'}} />", true),
    ] {
        let repo = jsx_repo(&before, &after, caller);
        let context = repo.run_json("context", &["--git"]);
        assert_eq!(
            has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
            blocking,
            "{context}"
        );
    }
    let repo = jsx_repo(
        &BEFORE.replace("options", "children"),
        &AFTER.replace("options", "children"),
        "<ControlPanel>{{key:'a'}}</ControlPanel>",
    );
    let context = repo.run_json("context", &["--git"]);
    assert!(
        has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
        "{context}"
    );
    let repo = jsx_repo(
        &BEFORE.replace("options?:", "children?: string; options?:"),
        &AFTER.replace("options?:", "children?: string; options?:"),
        "<ControlPanel>text</ControlPanel>",
    );
    let context = repo.run_json("context", &["--git"]);
    assert!(
        !has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
        "{context}"
    );
}

#[test]
fn impact_jsx_props_whitespace_children_keep_the_source_caller() {
    let before =
        "export function ControlPanel({ children }: { children?: string }) { return <span />; }\n";
    let after = before.replace("children?: string", "children?: number");
    for content in ["", " ", "\t", "\n"] {
        let caller = format!("<ControlPanel>{content}</ControlPanel>");
        let repo = jsx_repo(before, &after, &caller);
        let context = repo.run_json("context", &["--git"]);
        assert_eq!(
            has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
            !content.is_empty(),
            "{content:?}: {context}"
        );
        assert_eq!(
            has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
            content.is_empty(),
            "{content:?}: {context}"
        );
    }
}

#[test]
fn impact_jsx_props_default_and_unknown_contracts_keep_the_source_caller() {
    for (before, after) in [
        (BEFORE.replace("{ options }", "{ options = { key: 'a' } }"), AFTER.replace("{ options }", "{ options = { key: 'a' } }")),
        (BEFORE.replace("{ options }: { options?: { key: string } }", "props: { options?: { key: string } } = { options: { key: 'a' } }").replace("options?.key", "props.options?.key"), AFTER.replace("{ options }: { options?: { key: string; category: string } }", "props: { options?: { key: string; category: string } } = { options: { key: 'a' } }").replace("options?.key", "props.options?.key")),
        (BEFORE.to_string(), AFTER.replace("options?:", "options:")),
        (BEFORE.to_string(), AFTER.replace("options?:", "required: string; options?:")),
        (BEFORE.replace(") {", "): ReactNode {"), AFTER.replace(") {", "): ReactNode {")),
        (BEFORE.replace(") {", "): JSX.Element {"), AFTER.replace(") {", "): JSX.Element {")),
        (BEFORE.replace(") {", "): typeof result {"), AFTER.replace(") {", "): typeof result {")),
    ] {
        let repo = jsx_repo(&before, &after, "<ControlPanel />");
        let context = repo.run_json("context", &["--git"]);
        assert!(has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1), "{before} -> {after}: {context}");
        assert!(!has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1), "{context}");
    }
}

#[test]
fn impact_jsx_props_overloads_keep_the_source_caller() {
    let overload = "export function ControlPanel(props: { options?: { key: string } }): string;\n";
    let new_overload = "export function ControlPanel(props: { options?: { key: string; category: string }; label: string }): string;\n";
    let repo = jsx_repo(
        &format!(
            "{overload}{}",
            BEFORE.replace("return <span>{options?.key}</span>;", "return '';")
        ),
        &format!(
            "{new_overload}{}",
            AFTER.replace("return <span>{options?.key}</span>;", "return '';")
        ),
        "<ControlPanel />",
    );
    let context = repo.run_json("context", &["--git"]);
    assert!(
        has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
        "{context}"
    );
    assert!(
        !has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
        "{context}"
    );
}

#[test]
fn impact_jsx_props_inferred_return_depends_on_changed_props_stays_blocking() {
    let before = "export function ControlPanel({ options }: { options?: string }) { return options ?? null; }\n";
    let after = before.replace("options?: string", "options?: { key: string }");
    let repo = jsx_repo(before, &after, "<ControlPanel />");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
        "{context}"
    );
    assert!(
        !has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
        "{context}"
    );
    let impact = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert_eq!(impact.status.code(), Some(1), "{impact:?}");
    let review = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(review.status.code(), Some(1), "{review:?}");
    let hook: serde_json::Value = serde_json::from_slice(&review.stderr).unwrap();
    assert!(
        hook["api"]["mod"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["n"] == "ControlPanel"),
        "{hook}"
    );
    assert!(
        hook["impacts"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "{hook}"
    );
}

#[test]
fn impact_jsx_props_inferred_returns_keep_hooks_and_fragments_but_not_changed_categories() {
    for (before_body, after_body, informational) in [
        (
            "return <span />;",
            "useEffect(() => { return () => {}; }, []); return ((<><span /></>));",
            true,
        ),
        (
            "if (!options) return null; return <span />;",
            "if (!options) return null; return (<><span /></>);",
            true,
        ),
        (
            "const nested = () => { return null; }; return <span />;",
            "const nested = () => { return null; }; if (!options) return null; return <span />;",
            false,
        ),
        ("return <span />;", "return options;", false),
    ] {
        let before = format!(
            "import {{ useEffect }} from 'react';\n{}",
            BEFORE.replace("return <span>{options?.key}</span>;", before_body)
        );
        let after = format!(
            "import {{ useEffect }} from 'react';\n{}",
            AFTER.replace("return <span>{options?.key}</span>;", after_body)
        );
        let repo = jsx_repo(&before, &after, "<ControlPanel />");
        let context = repo.run_json("context", &["--git"]);
        assert_eq!(
            has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
            informational,
            "{after_body}: {context}"
        );
        assert_eq!(
            has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
            !informational,
            "{after_body}: {context}"
        );
    }
}

#[test]
fn impact_jsx_props_external_type_changes_do_not_hide_the_component_source() {
    for mode in ["local", "dependency", "binding"] {
        let repo = TestRepo::new();
        let old_other = "export type Other = { id: string };\n";
        let new_other = "export type Other = { id: string; extra: string };\n";
        repo.write("first.ts", old_other);
        repo.write("second.ts", new_other);
        let prefix = if mode == "local" {
            old_other
        } else {
            "import type { Other } from './first';\n"
        };
        let before = format!(
            "{prefix}{}",
            BEFORE.replace("options?:", "config: Other; options?:")
        );
        repo.write("panel.tsx", &before);
        repo.write("use.tsx", "import { ControlPanel } from './panel';\nexport function Page() { return <ControlPanel config={{id:'x'}} />; }\n");
        repo.init_git();
        repo.commit_all("initial");
        let prefix = match mode {
            "local" => new_other,
            "binding" => "import type { Other } from './second';\n",
            _ => prefix,
        };
        repo.write(
            "panel.tsx",
            format!(
                "{prefix}{}",
                AFTER.replace("options?:", "config: Other; options?:")
            ),
        );
        if mode == "dependency" {
            repo.write("first.ts", new_other);
        }
        let context = repo.run_json("context", &["--git"]);
        assert!(
            has_caller(&context, "panel.tsx", "impacted_callers", "use.tsx", 1),
            "{mode}: {context}"
        );
        assert!(
            !has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
            "{mode}: {context}"
        );
    }
}

#[test]
fn impact_jsx_props_same_named_sources_keep_low_priority_and_staged_input() {
    let repo = jsx_repo(BEFORE, BEFORE, "<ControlPanel />");
    repo.write("other.tsx", BEFORE);
    repo.write("other_use.tsx", "import { ControlPanel } from './other';\nexport function OtherPage() { return <ControlPanel />; }\n");
    repo.commit_all("second source");
    repo.write("panel.tsx", AFTER);
    repo.write("other.tsx", AFTER.replace("options?:", "options:"));
    let context = repo.run_json("context", &["--git"]);
    assert!(
        has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
        "{context}"
    );
    assert!(
        has_caller(
            &context,
            "other.tsx",
            "impacted_callers",
            "other_use.tsx",
            1
        ),
        "{context}"
    );
    assert!(
        has_caller(
            &context,
            "panel.tsx",
            "low_confidence_callers",
            "other_use.tsx",
            1
        ),
        "{context}"
    );
    assert!(
        !has_caller(
            &context,
            "panel.tsx",
            "informational_callers",
            "other_use.tsx",
            1
        ),
        "{context}"
    );
    repo.git(["add", "panel.tsx"]);
    repo.write("panel.tsx", AFTER.replace("options?:", "options:"));
    let staged = repo.run_json("context", &["--git", "--staged"]);
    assert!(
        has_caller(&staged, "panel.tsx", "informational_callers", "use.tsx", 1),
        "{staged}"
    );
    let worktree = repo.run_json("context", &["--git"]);
    assert!(
        has_caller(&worktree, "panel.tsx", "impacted_callers", "use.tsx", 1),
        "{worktree}"
    );
}

#[test]
fn impact_jsx_props_refs_and_dead_code_keep_their_occurrences() {
    let repo = jsx_repo(BEFORE, BEFORE, "<ControlPanel></ControlPanel>");
    let positions = |value: serde_json::Value| {
        value["refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| {
                serde_json::json!([
                    reference["path"],
                    reference["ln"],
                    reference["col"],
                    reference["kind"]
                ])
            })
            .collect::<Vec<_>>()
    };
    let before = positions(repo.run_json(
        "refs",
        &["--name", "ControlPanel", "--max-results", "unlimited"],
    ));
    assert_eq!(before.len(), 4);
    assert!(
        !repo
            .run_json("dead-code", &[])
            .to_string()
            .contains("ControlPanel")
    );
    repo.write("panel.tsx", AFTER);
    let after = positions(repo.run_json(
        "refs",
        &["--name", "ControlPanel", "--max-results", "unlimited"],
    ));
    assert_eq!(before, after);
    assert!(
        !repo
            .run_json("dead-code", &[])
            .to_string()
            .contains("ControlPanel")
    );
    let context = repo.run_json("context", &["--git"]);
    assert!(
        has_caller(&context, "panel.tsx", "informational_callers", "use.tsx", 1),
        "{context}"
    );
}
