use super::support::{TestRepo, cargo_bin};

const OLD_PRODUCER: &str = "export function cloneText(name: string): void { console.log(name); }\n";
const NEW_PRODUCER: &str =
    "export function cloneText(name: string): string[] { console.log(name); return [name]; }\n";
const OLD_HANDLERS: &str = "export interface Handlers { clone: (name: string) => void; }\nexport function execute(handlers: Handlers): number {\n  handlers.clone(\"x\"); return 0;\n}\n";
const NEW_HANDLERS: &str = "export interface Handlers { clone: (name: string) => readonly string[]; }\nexport function execute(handlers: Handlers): number {\n  return handlers.clone(\"x\").length;\n}\n";
const WIRING: &str = "import { cloneText } from './clone';\nimport { execute } from './handlers';\nexport function main(): number { return execute({ clone: cloneText }); }\n";

fn fixture(extension: &str, old_handlers: &str, new_handlers: &str, wiring: &str) -> TestRepo {
    fixture_with_extra(extension, old_handlers, new_handlers, wiring, None)
}

fn fixture_with_extra(
    extension: &str,
    old_handlers: &str,
    new_handlers: &str,
    wiring: &str,
    extra: Option<(&str, &str)>,
) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(format!("clone.{extension}"), OLD_PRODUCER);
    repo.write(format!("handlers.{extension}"), old_handlers);
    repo.write(format!("wiring.{extension}"), wiring);
    if let Some((path, source)) = extra {
        repo.write(path, source);
    }
    repo.commit_all("initial");
    repo.write(format!("clone.{extension}"), NEW_PRODUCER);
    repo.write(format!("handlers.{extension}"), new_handlers);
    repo
}

fn hook(repo: &TestRepo, staged: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let json = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    (output, json)
}

fn closed(json: &serde_json::Value, name: &str) -> bool {
    json["api"]["mod_closed"]
        .as_array()
        .is_some_and(|changes| changes.iter().any(|change| change["n"] == name))
}

#[test]
fn function_value_handoff_closes_the_exact_issue_and_keeps_occurrences() {
    for extension in ["ts", "tsx"] {
        let repo = fixture(extension, OLD_HANDLERS, NEW_HANDLERS, WIRING);
        let (output, json) = hook(&repo, false);
        assert!(output.status.success(), "{extension}: {json}");
        assert!(
            closed(&json, "cloneText") && closed(&json, "Handlers"),
            "{json}"
        );
        assert!(json["impacts"].is_null(), "{json}");
        assert!(
            json["blocking_categories"].as_array().unwrap().is_empty(),
            "{json}"
        );
        let references = repo.run_json("refs", &["--name", "cloneText"]);
        assert!(
            references["refs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reference| {
                    reference["path"] == format!("wiring.{extension}") && reference["ln"] == 2
                }),
            "{references}"
        );
        let output = cargo_bin()
            .args(["impact", "--git", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn function_value_handoff_accepts_multiline_and_all_unchanged_handoffs() {
    let wiring = "import { cloneText } from './clone';\nimport { execute } from './handlers';\nexport function main(): number {\n  execute(\n    { /* callback */ clone: cloneText },\n  );\n  return execute({ clone: cloneText });\n}\n";
    let repo = fixture("ts", OLD_HANDLERS, NEW_HANDLERS, wiring);
    let (output, json) = hook(&repo, false);
    assert!(output.status.success(), "{json}");
    assert!(
        closed(&json, "cloneText") && closed(&json, "Handlers"),
        "{json}"
    );
}

#[test]
fn function_value_handoff_accepts_same_file_and_discarded_result() {
    let repo = TestRepo::new();
    repo.init_git();
    let call = "export function main(): number { return execute({ clone: cloneText }); }\n";
    repo.write("api.ts", format!("{OLD_PRODUCER}{OLD_HANDLERS}{call}"));
    repo.commit_all("initial");
    let handlers = NEW_HANDLERS.replace(
        "return handlers.clone(\"x\").length;",
        "handlers.clone(\"x\"); return 0;",
    );
    repo.write("api.ts", format!("{NEW_PRODUCER}{handlers}{call}"));
    let (output, json) = hook(&repo, false);
    assert!(output.status.success(), "{json}");
    assert!(
        closed(&json, "cloneText") && closed(&json, "Handlers"),
        "{json}"
    );
}

#[test]
fn function_value_handoff_requires_the_changed_receiver_and_no_direct_call() {
    for wiring in [
        WIRING.to_owned(),
        WIRING.replace("return execute", "cloneText('extra'); return execute"),
        WIRING.replace("{ clone: cloneText }", "{ clone: cloneText('x') }"),
    ] {
        let unchanged_interface = wiring == WIRING;
        let handlers = if unchanged_interface {
            OLD_HANDLERS
        } else {
            NEW_HANDLERS
        };
        let repo = fixture("ts", OLD_HANDLERS, handlers, &wiring);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{json}");
        assert!(!closed(&json, "cloneText"), "{json}");
        assert!(!json["impacts"].is_null(), "{json}");
    }
}

#[test]
fn function_value_handoff_rejects_unbounded_consumer_dependencies() {
    for (old, new) in [
        (
            OLD_HANDLERS.replace(": number {", " {"),
            NEW_HANDLERS.replace(": number {", " {"),
        ),
        (
            OLD_HANDLERS.replace(
                "handlers.clone(\"x\"); return 0;",
                "return acceptsVoid(handlers.clone(\"x\"));",
            ) + "function acceptsVoid(value: void): number { return 0; }\n",
            NEW_HANDLERS.replace(
                "return handlers.clone(\"x\").length;",
                "return acceptsVoid(handlers.clone(\"x\"));",
            ) + "function acceptsVoid(value: void): number { return 0; }\n",
        ),
        (
            OLD_HANDLERS.replace(
                "handlers.clone(\"x\");",
                "const callback = handlers.clone; callback(\"x\");",
            ),
            NEW_HANDLERS.replace(
                "return handlers.clone(\"x\").length;",
                "const callback = handlers.clone; return callback(\"x\").length;",
            ),
        ),
        (
            OLD_HANDLERS.replace(
                "handlers.clone(\"x\");",
                "handlers.clone = fallback; handlers.clone(\"x\");",
            ) + "function fallback(name: string): void {}\n",
            NEW_HANDLERS.replace(
                "return handlers.clone(\"x\").length;",
                "handlers.clone = fallback; return handlers.clone(\"x\").length;",
            ) + "function fallback(name: string): void {}\n",
        ),
    ] {
        let repo = fixture("ts", &old, &new, WIRING);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{json}");
        assert!(!closed(&json, "cloneText"), "{json}");
    }
}

#[test]
fn function_value_handoff_rejects_unsupported_handoff_shapes_and_bindings() {
    for wiring in [
        WIRING.replace("{ clone: cloneText }", "{ ['clone']: cloneText }"),
        WIRING.replace("{ clone: cloneText }", "{ 'clone': cloneText }"),
        WIRING.replace(
            "{ clone: cloneText }",
            "{ clone: cloneText, clone: cloneText }",
        ),
        WIRING.replace("{ clone: cloneText }", "{ ...{ clone: cloneText } }"),
        WIRING.replace(
            "{ clone: cloneText }",
            "{ clone: cloneText as typeof cloneText }",
        ),
        WIRING.replace("{ clone: cloneText }", "{ clone: alias }") + "const alias = cloneText;\n",
        WIRING.replace(
            "export function main(): number",
            "export function main(cloneText: never): number",
        ),
        WIRING
            .replace("import { cloneText }", "import { cloneText as alias }")
            .replace("clone: cloneText", "clone: alias"),
        WIRING
            .replace("import { cloneText }", "import * as module")
            .replace("clone: cloneText", "clone: module['clone' + 'Text']"),
        WIRING.replace("execute({", "execute?.({"),
        WIRING.replace("execute({", "execute<string>({"),
        WIRING.to_owned() + "type Copied = typeof cloneText;\n",
        WIRING.to_owned() + "export { cloneText };\n",
    ] {
        let repo = fixture("ts", OLD_HANDLERS, NEW_HANDLERS, &wiring);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{wiring}: {json}");
        assert!(!closed(&json, "cloneText"), "{wiring}: {json}");
    }
}

#[test]
fn function_value_handoff_rejects_resolution_or_inventory_uncertainty() {
    for (path, source) in [
        ("tsconfig.json", "{\"extends\":\"./base.json\"}"),
        (
            "tsconfig.json",
            "{\"compilerOptions\":{\"rootDirs\":[\".\",\"other\"]}}",
        ),
        (
            "tsconfig.json",
            "{\"compilerOptions\":{\"moduleSuffixes\":[\".native\",\"\"]}}",
        ),
        (
            "extra.ts",
            "import * as producerModule from './clone';\nproducerModule['clone' + 'Text']('x');\n",
        ),
        ("extra.ts", "export * from './clone';\n"),
        ("extra.ts", "eval('import(\"./clone\")');\n"),
        ("extra.ts", "import('./clone');\n"),
        ("extra.ts", "export function broken( {\n"),
        (
            "extra.vue",
            "<script setup>import { cloneText } from './clone';</script>\n",
        ),
        (
            "clone.tsx",
            "export function cloneText(name: string): void {}\n",
        ),
    ] {
        let repo = fixture_with_extra(
            "ts",
            OLD_HANDLERS,
            NEW_HANDLERS,
            WIRING,
            Some((path, source)),
        );
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{path}: {json}");
        assert!(!closed(&json, "cloneText"), "{path}: {json}");
    }
}

#[test]
fn function_value_handoff_preserves_another_causal_symbol_on_the_same_line() {
    let wiring = WIRING
        .replace(
            "export function main",
            "import { other } from './other';\nexport function main",
        )
        .replace(
            "return execute({ clone: cloneText });",
            "execute({ clone: cloneText }); return other();",
        );
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("clone.ts", OLD_PRODUCER);
    repo.write("handlers.ts", OLD_HANDLERS);
    repo.write("wiring.ts", wiring);
    repo.write(
        "other.ts",
        "export function other(): number { return 0; }\n",
    );
    repo.commit_all("initial");
    repo.write("clone.ts", NEW_PRODUCER);
    repo.write("handlers.ts", NEW_HANDLERS);
    repo.write(
        "other.ts",
        "export function other(): string { return 'changed'; }\n",
    );
    let (output, json) = hook(&repo, false);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(
        closed(&json, "cloneText") && closed(&json, "Handlers"),
        "{json}"
    );
    assert!(
        json["impacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|group| group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol == "other")),
        "{json}"
    );
}

#[test]
fn function_value_handoff_staged_uses_the_receiver_from_the_index() {
    let repo = fixture("ts", OLD_HANDLERS, OLD_HANDLERS, WIRING);
    repo.stage_all();
    repo.write("handlers.ts", NEW_HANDLERS);
    let (output, json) = hook(&repo, true);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(!closed(&json, "cloneText"), "{json}");
    repo.stage_all();
    let (output, json) = hook(&repo, true);
    assert!(output.status.success(), "{json}");
    assert!(
        closed(&json, "cloneText") && closed(&json, "Handlers"),
        "{json}"
    );
}
