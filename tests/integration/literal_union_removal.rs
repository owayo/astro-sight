//! 文字列 union の削除と、限定した値の流れの閉包を確認する。

use super::support::{TestRepo, cargo_bin};

#[test]
fn discriminated_union_removal_closes_updated_branches_and_literals() {
    for extension in ["ts", "tsx"] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        let messages = format!("src/messages.{extension}");
        let handler = format!("src/handler.{extension}");
        repo.write(
            &messages,
            "export type Message = { type: 'open' } | { type: 'close' };\n",
        );
        let before = "import type { Message } from './messages';\nexport function handle(message: Message): string {\n if (message.type === 'open') return 'opened';\n if (message.type === 'close') return 'closed';\n return 'other';\n}\n";
        repo.write(&handler, before);
        repo.write(format!("src/sender.{extension}"), "import { handle } from './handler';\nexport const result = handle({ type: 'close' });\n");
        repo.commit_all("initial");
        repo.write(&messages, "export type Message = { type: 'close' };\n");
        repo.write(
            &handler,
            before.replace(" if (message.type === 'open') return 'opened';\n", ""),
        );
        let normal = repo.run_json("review", &["--git"]);
        assert!(
            normal["api_changes"]["modified_closed_in_diff"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["name"] == "Message")),
            "{normal}"
        );
        let (code, json) = union_hook(&repo, false, false);
        assert_eq!(code, 0, "{json}");
        assert_eq!(json["api"]["mod_closed"][0]["n"], "Message", "{json}");
        repo.stage_all();
        repo.write(&handler, before);
        let (code, json) = union_hook(&repo, true, false);
        assert_eq!(code, 0, "{json}");
        assert_eq!(json["api"]["mod_closed"][0]["n"], "Message", "{json}");
    }
}

#[test]
fn discriminated_union_removal_checks_switches_payloads_and_unsupported_flows() {
    for (body, argument, variant, expected) in [
        (
            "switch (message.type) { case 'close': return 'closed'; default: return 'other'; }",
            "{ type: 'close' }",
            "{ type: 'close' }",
            0,
        ),
        (
            "if ('close' === message.type) return 'closed'; return 'other';",
            "{ type: 'close' }",
            "{ type: 'close' }",
            0,
        ),
        (
            "return message.type === 'close' ? 'closed' : 'other';",
            "{ type: 'close', count: 2, ready: true }",
            "{ readonly type: 'close'; count: number; ready: boolean }",
            0,
        ),
        (
            "if (message.type === 'open') return 'opened'; return 'other';",
            "{ type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "switch (message.type) { case 'open': return 'opened'; default: return 'other'; }",
            "{ type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "return message.type === 'close' ? 'closed' : 'other';",
            "{ type: 'open' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "return message.type === 'close' ? 'closed' : 'other';",
            "{ type: 'close', count: 'wrong' }",
            "{ type: 'close'; count: number }",
            1,
        ),
        (
            "return message.type === 'close' ? 'closed' : 'other';",
            "{ type: 'close' }",
            "{ type: 'close'; count: number }",
            1,
        ),
        (
            "const alias = message; return 'other';",
            "{ type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "consume(message); return 'other';",
            "{ type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "message.type = 'close'; return 'other';",
            "{ type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "type Original = typeof message; return 'other';",
            "{ type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "return message['type'] === 'close' ? 'closed' : 'other';",
            "{ type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
        (
            "return message.type === 'close' ? 'closed' : 'other';",
            "({ type: 'close' } as unknown)",
            "{ type: 'close' }",
            1,
        ),
        (
            "return message.type === 'close' ? 'closed' : 'other';",
            "{ ...{}, type: 'close' }",
            "{ type: 'close' }",
            1,
        ),
    ] {
        discriminated_union_case("ts", body, argument, variant, expected);
    }
    discriminated_union_case(
        "tsx",
        "switch (message.type) { case 'close': return 'closed'; default: return 'other'; }",
        "{ type: 'close', text: 'value' }",
        "{ type: 'close'; text: string }",
        0,
    );
}

fn discriminated_union_case(
    extension: &str,
    body: &str,
    argument: &str,
    variant: &str,
    expected: i32,
) {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let messages = format!("src/messages.{extension}");
    let handler = format!("src/handler.{extension}");
    let sender = format!("src/sender.{extension}");
    let before = format!(
        "import type {{ Message }} from './messages';\ndeclare function consume(value: unknown): void;\nexport function handle(message: Message): string {{\n if (message.type === 'open') return 'opened';\n {body}\n}}\n"
    );
    let after = before.replace(" if (message.type === 'open') return 'opened';\n", "");
    repo.write(
        &messages,
        format!("export type Message = {{ type: 'open' }} | {variant};\n"),
    );
    repo.write(&handler, &before);
    repo.write(
        &sender,
        format!(
            "import {{ handle }} from './handler';\nexport const result = handle({argument});\n"
        ),
    );
    repo.commit_all("initial");
    repo.write(&messages, format!("export type Message = {variant};\n"));
    repo.write(&handler, &after);
    let normal = repo.run_json("review", &["--git"]);
    let bucket = if expected == 0 {
        "modified_closed_in_diff"
    } else {
        "modified"
    };
    assert!(
        normal["api_changes"][bucket]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry["name"] == "Message")),
        "{body}/{argument}/{variant}: {normal}"
    );
    let (code, json) = union_hook(&repo, false, false);
    assert_eq!(code, expected, "{body}/{argument}/{variant}: {json}");
    repo.stage_all();
    repo.write(&handler, &before);
    let (code, json) = union_hook(&repo, true, false);
    assert_eq!(code, expected, "{body}/{argument}/{variant}: {json}");
}

const BEFORE_EXPANSION: &str = r#"export type Mode = "early" | "seen";
export function modesOf(n: number): Mode[] {
    const modes: Mode[] = [];
    if (n < 0) modes.push("early");
    if (n === 0) modes.push("seen");
    return modes;
}
"#;
const INCOMPLETE_SWITCH: &str = r#"import type { Mode } from "./mode";
export function describe(mode: Mode): string {
    switch (mode) {
        case "early": return "before the start";
        case "seen": return "already seen";
    }
}
"#;
const NEW_CASE: &str = "        case \"filtered\": return \"filtered out\";\n";

fn expand_modes(repo: &TestRepo) {
    repo.write(
        "src/mode.ts",
        BEFORE_EXPANSION
            .replace("| \"seen\";", "| \"seen\" | \"filtered\";")
            .replace(
                "    return modes;",
                "    if (n > 100) modes.push(\"filtered\");\n    return modes;",
            ),
    );
}

fn complete_switch() -> String {
    INCOMPLETE_SWITCH.replace("    }\n}", &format!("{NEW_CASE}    }}\n}}"))
}

#[test]
fn literal_union_addition_checks_updated_switches_in_worktree_and_index() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/mode.ts", BEFORE_EXPANSION);
    repo.write("src/describe.ts", INCOMPLETE_SWITCH);
    repo.commit_all("initial");
    expand_modes(&repo);
    repo.write("src/describe.ts", complete_switch());

    let (code, json) = union_hook(&repo, false, false);
    union_category(code, &json, 0, true, "all cases");
    repo.stage_all();
    repo.write("src/describe.ts", INCOMPLETE_SWITCH);
    let (code, json) = union_hook(&repo, true, false);
    union_category(code, &json, 0, true, "index version has all cases");
    let (code, json) = union_hook(&repo, false, false);
    union_category(code, &json, 1, false, "case missing from worktree");
    assert_eq!(
        json["api"]["mod"][0]["unverified"]["reason"], "unsupported_type_change",
        "{json}"
    );
}

#[test]
fn literal_union_addition_checks_every_reader_and_untyped_producer_references() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/mode.ts", BEFORE_EXPANSION);
    repo.write("src/describe.ts", INCOMPLETE_SWITCH);
    repo.write(
        "src/describe_other.ts",
        INCOMPLETE_SWITCH.replace("describe(", "describeOther("),
    );
    repo.write("src/app.ts", "export const ready = true;\n");
    repo.commit_all("initial");
    expand_modes(&repo);
    repo.write("src/describe.ts", complete_switch());
    let (code, json) = union_hook(&repo, false, false);
    union_category(code, &json, 1, false, "second switch misses a case");

    repo.write("src/describe_other.ts", "import type { Mode } from './mode';\nexport function describeOther(mode: Mode): boolean { const values: string[] = ['early', 'seen']; return values.includes(mode); }\n");
    let (code, json) = union_hook(&repo, false, false);
    union_category(code, &json, 1, false, "non-exhaustive membership check");

    repo.write(
        "src/describe_other.ts",
        complete_switch().replace("describe(", "describeOther("),
    );
    let (code, json) = union_hook(&repo, false, false);
    union_category(code, &json, 0, true, "both switches cover all cases");

    repo.write("src/app.ts", "import { modesOf } from './mode';\nexport const oldModes: ('early' | 'seen')[] = modesOf(101);\n");
    let (code, json) = union_hook(&repo, false, false);
    union_category(
        code,
        &json,
        1,
        false,
        "unproved receiver of widened producer result",
    );
}

#[test]
fn literal_union_addition_proves_never_from_an_exhaustive_switch() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/mode.ts", BEFORE_EXPANSION);
    repo.write(
        "src/describe.ts",
        "import type { Mode } from './mode';\nexport function describe(mode: Mode): string { if (mode === 'early') return 'early'; return 'seen'; }\nexport const caption = describe('early');\n",
    );
    repo.commit_all("initial");
    expand_modes(&repo);
    let with_never = complete_switch().replace(
        "    }\n}",
        "        default: { const exhaustive: never = mode; return exhaustive; }\n    }\n}\nexport const caption = describe('early');",
    );
    for (case, source, expected) in [
        ("if replaced by complete switch", with_never.clone(), 0),
        (
            "never assertion",
            with_never.replace("= mode;", "= mode as never;"),
            1,
        ),
        (
            "type-check suppression",
            format!("// @ts-nocheck\n{with_never}"),
            1,
        ),
        (
            "incomplete switch with never",
            with_never.replace(NEW_CASE, ""),
            1,
        ),
    ] {
        repo.write("src/describe.ts", source);
        let (code, json) = union_hook(&repo, false, false);
        union_category(code, &json, expected, expected == 0, case);
    }
}

const IMPORT_MODE: &str = "import type { Mode } from \"./mode\";\n";
const SEEN_CASE: &str = "case \"seen\": return \"already seen\";";
const ROOT_ARROW: &str = "export const describe = (mode: Mode): string => {\n    switch (mode) {\n        case \"early\": return \"before the start\";\n        case \"seen\": return \"already seen\";\n    }\n};\n";
const LOCAL_FUNCTION: &str = "export function label(mode: Mode): string {\n    function describe(value: Mode): string {\n        switch (value) {\n            case \"early\": return \"before the start\";\n            case \"seen\": return \"already seen\";\n        }\n    }\n    return describe(mode);\n}\n";
const LOCAL_ARROW: &str = "export function label(mode: Mode): string {\n    const describe = (value: Mode): string => {\n        switch (value) {\n            case \"early\": return \"before the start\";\n            case \"seen\": return \"already seen\";\n        }\n    };\n    return describe(mode);\n}\n";

/// `src/describe.ts` を `reader` にしてコミットし、union に要素を足す。`complete` なら読む関数の
/// switch にも同じ差分で case を足す。`app` があれば `src/app.ts` として置く。
fn reader_hook(reader: &str, complete: bool, app: Option<&str>) -> (i32, serde_json::Value) {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/mode.ts", BEFORE_EXPANSION);
    repo.write("src/describe.ts", reader);
    if let Some(app) = app {
        repo.write("src/app.ts", app);
    }
    repo.commit_all("initial");
    expand_modes(&repo);
    if complete {
        repo.write(
            "src/describe.ts",
            reader.replace(
                SEEN_CASE,
                &format!("{SEEN_CASE} case \"filtered\": return \"filtered out\";"),
            ),
        );
    }
    union_hook(&repo, false, false)
}

#[test]
fn literal_union_addition_accepts_arrow_and_local_readers() {
    let hoisted = "export function label(mode: Mode): string {\n    return describe(mode);\n    function describe(value: Mode): string {\n        switch (value) {\n            case \"early\": return \"before the start\";\n            case \"seen\": return \"already seen\";\n        }\n    }\n}\n";
    let two_scopes = format!(
        "{LOCAL_FUNCTION}{}",
        LOCAL_FUNCTION.replace("label(", "caption(")
    );
    let literal_only = "export function make(fallback: string): string {\n    function describe(value: Mode): string {\n        switch (value) {\n            case \"early\": return \"before the start\";\n            case \"seen\": return \"already seen\";\n        }\n    }\n    return describe(\"seen\") + fallback;\n}\n";
    let imported =
        "import { describe } from \"./describe\";\nexport const caption = describe(\"early\");\n";
    let own_arguments = "export function describe(mode: Mode): string {\n    function first(): unknown {\n        return arguments[0];\n    }\n    void first;\n    switch (mode) {\n        case \"early\": return \"before the start\";\n        case \"seen\": return \"already seen\";\n    }\n}\n";
    let computed_literal = ROOT_ARROW.replace(
        "    switch (mode) {",
        "    const handlers = { [publish(\"early\")](mode: number) { return mode; } };\n    void handlers;\n    switch (mode) {",
    ) + "function publish(value: \"early\" | \"seen\"): string {\n    return value;\n}\n";
    for (case, body, app, check_missing) in [
        ("root const arrow", ROOT_ARROW, None, true),
        (
            "root const arrow imported",
            ROOT_ARROW,
            Some(imported),
            false,
        ),
        ("local function", LOCAL_FUNCTION, None, true),
        ("local const arrow", LOCAL_ARROW, None, true),
        (
            "local function called before its declaration",
            hoisted,
            None,
            false,
        ),
        (
            "same local name in two functions",
            two_scopes.as_str(),
            None,
            true,
        ),
        (
            "local function called with a literal",
            literal_only,
            None,
            false,
        ),
        (
            "inner function reading its own arguments",
            own_arguments,
            None,
            false,
        ),
        (
            "computed method name without the value",
            computed_literal.as_str(),
            None,
            false,
        ),
    ] {
        let reader = format!("{IMPORT_MODE}{body}");
        let (code, json) = reader_hook(&reader, true, app);
        union_category(code, &json, 0, true, case);
        if check_missing {
            let (code, json) = reader_hook(&reader, false, app);
            union_category(
                code,
                &json,
                1,
                false,
                &format!("{case} without the new case"),
            );
        }
    }
}

#[test]
fn literal_union_addition_keeps_unproven_arrow_and_local_readers_blocking() {
    let complete_cases = "            case \"early\": return \"before the start\";\n            case \"seen\": return \"already seen\";\n";
    let root_function = "export function describe(mode: Mode): string {\n    const first: unknown = arguments[0];\n    switch (mode) {\n        case \"early\": return \"before the start\";\n        case \"seen\": return \"already seen\";\n    }\n}\n".to_owned();
    for (case, body) in [
        (
            "let arrow",
            ROOT_ARROW.replace("export const describe", "export let describe"),
        ),
        (
            "escaped arrow",
            format!("{ROOT_ARROW}export const alias = describe;\n"),
        ),
        (
            "returned local function",
            format!(
                "export function getReader() {{\n    function describe(value: Mode): string {{\n        const describe = 0;\n        switch (value) {{\n{complete_cases}        }}\n    }}\n    return describe;\n}}\n"
            ),
        ),
        (
            "reassigned local function",
            LOCAL_FUNCTION.replace(
                "    return describe(mode);",
                "    describe = undefined!;\n    return describe(mode);",
            ),
        ),
        ("arguments of a root function", root_function.clone()),
        (
            "arguments read by an inner arrow without the type",
            root_function.replace(
                "    const first: unknown = arguments[0];\n",
                "    const first = (): unknown => arguments[0];\n    void first;\n",
            ),
        ),
        (
            "shorthand arguments",
            root_function.replace("arguments[0]", "({ arguments }).arguments[0]"),
        ),
        (
            "arguments in a computed method name",
            root_function.replace(
                "    const first: unknown = arguments[0];\n",
                "    const keys = { [String(arguments[0])]() { return 0; } };\n    void keys;\n",
            ),
        ),
        (
            "outer arguments read by a local arrow",
            LOCAL_ARROW.replace(
                "        switch (value)",
                "        const first: unknown = arguments[0];\n        switch (value)",
            ),
        ),
        (
            "var of the same name",
            LOCAL_FUNCTION
                .replace("label(mode: Mode)", "label(mode: Mode, flag: boolean)")
                .replace(
                    "    return describe(mode);",
                    "    if (flag) { var describe = 0; }\n    return describe(mode);",
                ),
        ),
        (
            "value passed through a computed method name",
            ROOT_ARROW.replace(
                "    switch (mode) {",
                "    const handlers = { [publish(mode)](mode: number) { return mode; } };\n    void handlers;\n    switch (mode) {",
            ) + "function publish(value: \"early\" | \"seen\"): string {\n    return value;\n}\n",
        ),
        (
            "value passed through a for header",
            ROOT_ARROW.replace(
                "    switch (mode) {",
                "    for (; publish(mode);) { const mode = 0; void mode; break; }\n    switch (mode) {",
            ) + "function publish(value: \"early\" | \"seen\"): boolean {\n    return value === \"early\";\n}\n",
        ),
        (
            "value passed through a parameter decorator",
            ROOT_ARROW.replace(
                "    switch (mode) {",
                "    class Handler { run(@mark(mode) mode: number) { return mode; } }\n    void Handler;\n    switch (mode) {",
            ) + "declare function mark(value: \"early\" | \"seen\"): ParameterDecorator;\n",
        ),
        (
            "arguments in a parameter decorator",
            root_function.replace(
                "    const first: unknown = arguments[0];\n",
                "    class Handler { run(@mark(arguments[0]) value: number) { return value; } }\n    void Handler;\n",
            ) + "declare function mark(value: unknown): ParameterDecorator;\n",
        ),
        (
            // for-of の左辺の既定値の名前は、ループ本体の宣言へ解決されうるので確かめられない。
            "reader called in a for-of default",
            LOCAL_FUNCTION.replace(
                "    return describe(mode);",
                "    let value = \"\";\n    const rows: [string?][] = [[]];\n    for ([value = describe(\"seen\")] of rows) {}\n    void value;\n    return describe(mode);",
            ),
        ),
        (
            "local function captured by an inner parameter initializer",
            format!(
                "export function makeReader() {{\n    function describe(value: Mode): string {{\n        switch (value) {{\n{complete_cases}        }}\n    }}\n    function capture(saved = describe) {{\n        const describe = 0;\n        void describe;\n        return saved;\n    }}\n    return capture();\n}}\n"
            ),
        ),
        (
            "name in a parameter initializer",
            format!(
                "export function make(fallback: string = describe(\"early\")): string {{\n    function describe(value: Mode): string {{\n        switch (value) {{\n{complete_cases}        }}\n    }}\n    return describe(\"seen\") + fallback;\n}}\n"
            ),
        ),
        (
            "for-of write to a local function from an inner function",
            LOCAL_FUNCTION.replace(
                "    return describe(mode);",
                "    function reset(): void {\n        const replacements: never[] = [];\n        for (describe of replacements) {}\n    }\n    reset();\n    return describe(mode);",
            ),
        ),
        (
            "for-of write to a root function",
            format!(
                "{}const replacements: never[] = [];\nfor (describe of replacements) {{}}\n",
                INCOMPLETE_SWITCH.replace("import type { Mode } from \"./mode\";\n", "")
            ),
        ),
        (
            "function expression",
            ROOT_ARROW.replace(
                "(mode: Mode): string => {",
                "function (mode: Mode): string {",
            ),
        ),
        (
            "type parameters",
            ROOT_ARROW.replace("= (mode: Mode)", "= <T,>(mode: Mode)"),
        ),
    ] {
        let reader = format!("{IMPORT_MODE}{body}");
        let (code, json) = reader_hook(&reader, true, None);
        union_category(code, &json, 1, false, case);
    }
}

/// `src/report.ts` を `report` にしてコミットし、union に要素を足す。`complete` なら
/// `src/describe.ts` の switch にも case を足す。`describe` があれば読む関数をそれにする。
fn producer_hook(report: &str, complete: bool, describe: Option<&str>) -> (i32, serde_json::Value) {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let reader = describe.unwrap_or(INCOMPLETE_SWITCH);
    repo.write("src/mode.ts", BEFORE_EXPANSION);
    repo.write("src/describe.ts", reader);
    repo.write("src/report.ts", report);
    repo.commit_all("initial");
    expand_modes(&repo);
    if complete {
        repo.write(
            "src/describe.ts",
            reader.replace(SEEN_CASE, &format!("{SEEN_CASE}\n{NEW_CASE}")),
        );
    }
    union_hook(&repo, false, false)
}

/// 生成関数の直接の呼び出しは、結果を `.length` で読むか、`.filter(...)` の連鎖と最後の
/// `.map(...)` で証明済みの受け口へ渡す形だけを許す。
#[test]
fn literal_union_addition_accepts_closed_producer_calls() {
    let imports = "import { modesOf } from \"./mode\";\nimport { describe } from \"./describe\";\n";
    for (case, body, expected) in [
        (
            "map with the reader",
            "export function report(n: number): string {\n    return modesOf(n).map(describe).join(\", \");\n}\n",
            0,
        ),
        (
            "map with an arrow",
            "export function report(n: number): string[] {\n    return modesOf(n).map((mode) => describe(mode));\n}\n",
            0,
        ),
        ("length", "export const count = modesOf(1).length;\n", 0),
        (
            "filter then map",
            "export function report(n: number): string[] {\n    return modesOf(n).filter((mode) => mode !== \"early\").map(describe);\n}\n",
            0,
        ),
        (
            "array kept in a variable",
            "export function report(n: number): number {\n    const modes = modesOf(n);\n    return modes.length;\n}\n",
            1,
        ),
        (
            "array returned",
            "export function report(n: number) {\n    return modesOf(n);\n}\n",
            1,
        ),
        (
            "array passed to an unknown function",
            "declare function keep(value: unknown): void;\nexport function report(n: number): void {\n    keep(modesOf(n));\n}\n",
            1,
        ),
        (
            "map with a function that is not a reader",
            "export function report(n: number): string[] {\n    return modesOf(n).map(String);\n}\n",
            1,
        ),
        (
            "map returning the element",
            "export function report(n: number) {\n    return modesOf(n).map((mode) => mode);\n}\n",
            1,
        ),
        (
            "map with thisArg",
            "export function report(n: number): string[] {\n    return modesOf(n).map(describe, null);\n}\n",
            1,
        ),
        (
            "filter result returned",
            "export function report(n: number) {\n    return modesOf(n).filter((mode) => mode !== \"early\");\n}\n",
            1,
        ),
        (
            "length written",
            "export function report(n: number): void {\n    modesOf(n).length++;\n}\n",
            1,
        ),
        ("type query", "export type Producer = typeof modesOf;\n", 1),
        (
            "reader passed to an unknown array as well",
            "export function report(n: number): string[] {\n    return modesOf(n).map(describe);\n}\nexport const others = JSON.parse(\"[]\").map(describe);\n",
            1,
        ),
    ] {
        let (code, json) = producer_hook(&format!("{imports}{body}"), true, None);
        union_category(code, &json, expected, expected == 0, case);
    }

    // 同じ呼び出し形で、読む関数の条件だけを変える。絞り込んでも、読む関数には全要素の case を求める。
    let mapped = format!(
        "{imports}export function report(n: number): string[] {{\n    return modesOf(n).filter((mode) => mode !== \"early\").map(describe);\n}}\n"
    );
    let overloaded = INCOMPLETE_SWITCH.replace(
        "export function describe(mode: Mode): string {",
        "export function describe(mode: Mode): string;\nexport function describe(mode: Mode): string {",
    );
    let reads_arguments = INCOMPLETE_SWITCH.replace(
        "    switch (mode) {",
        "    const first: unknown = arguments[0];\n    void first;\n    switch (mode) {",
    );
    for (case, complete, reader, expected) in [
        ("reader with every case", true, None, 0),
        ("reader without the new case", false, None, 1),
        ("overloaded reader", true, Some(overloaded.as_str()), 1),
        (
            "reader reading arguments",
            true,
            Some(reads_arguments.as_str()),
            1,
        ),
    ] {
        let (code, json) = producer_hook(&mapped, complete, reader);
        union_category(code, &json, expected, expected == 0, case);
    }

    let typed_imports = "import { type Mode, modesOf } from \"./mode\";\nimport { describe } from \"./describe\";\n";
    for (case, report) in [
        (
            "aliased producer import",
            "import { modesOf as reasons } from \"./mode\";\nimport { describe } from \"./describe\";\nexport function report(n: number): string[] {\n    return reasons(n).map(describe);\n}\n".to_owned(),
        ),
        (
            "length after a filter",
            format!("{imports}export const count = modesOf(1).filter((mode) => mode !== \"early\").length;\n"),
        ),
        (
            "outer parameter compared in a filter",
            format!("{typed_imports}export function report(mode: Mode): string {{\n    return modesOf(1).filter((_value) => mode === \"early\").map(describe).join(\",\");\n}}\n"),
        ),
        (
            "producer called in a for-of default",
            format!("{imports}export function report(): (\"early\" | \"seen\")[] {{\n    let result: (\"early\" | \"seen\")[] = [];\n    const rows: [undefined][] = [[undefined]];\n    for ([result = modesOf(101)] of rows) {{\n        const modesOf = (): never[] => [];\n        void modesOf;\n    }}\n    return result;\n}}\n"),
        ),
        (
            "type-check suppression in the caller",
            format!("{imports}// @ts-expect-error \"filtered\" は旧 union にない\nexport const report = modesOf(101).filter((mode) => mode !== \"filtered\").map(describe);\n"),
        ),
    ] {
        let (code, json) = producer_hook(&report, true, None);
        union_category(code, &json, 1, false, case);
    }
}

/// 別のファイルの別名の export の範囲が生成関数の宣言と同じでも、宣言とみなさない。
#[test]
fn literal_union_addition_does_not_match_declarations_across_files() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/mode.ts", BEFORE_EXPANSION);
    repo.write("src/describe.ts", INCOMPLETE_SWITCH);
    repo.write(
        "src/bridge.ts",
        "import { modesOf } from \"./mode\";\nexport { modesOf as forwarded };\n",
    );
    repo.write(
        "src/caller.ts",
        "import { forwarded } from \"./bridge\";\nexport const old: (\"early\" | \"seen\")[] = forwarded(101);\n",
    );
    repo.commit_all("initial");
    expand_modes(&repo);
    repo.write("src/describe.ts", complete_switch());
    // export specifier `modesOf as <alias>` の範囲を、生成関数の宣言の範囲にそろえる。
    let mode = std::fs::read_to_string(repo.root().join("src/mode.ts")).unwrap();
    let start = mode.find("function modesOf").unwrap();
    let end = mode.rfind('}').unwrap() + 1;
    let import = "import { modesOf } from \"./mode\";\n";
    let alias = "a".repeat(end - start - "modesOf as ".len());
    let padding = " ".repeat(start - import.len() - "export { ".len());
    repo.write(
        "src/bridge.ts",
        format!("{import}{padding}export {{ modesOf as {alias} }};\n"),
    );
    repo.write(
        "src/caller.ts",
        format!("import {{ {alias} }} from \"./bridge\";\nexport const old: (\"early\" | \"seen\")[] = {alias}(101);\n"),
    );
    let bridge = std::fs::read_to_string(repo.root().join("src/bridge.ts")).unwrap();
    assert_eq!(
        &bridge[start..end],
        format!("modesOf as {alias}"),
        "specifier range"
    );
    let (code, json) = union_hook(&repo, false, false);
    union_category(
        code,
        &json,
        1,
        false,
        "aliased export with the declaration range",
    );
}

/// 判別 union の要素削除でも、`const` に代入した矢印関数を受け口として扱う。
#[test]
fn discriminated_union_removal_accepts_arrow_consumers() {
    let before = "import type { Message } from './messages';\nexport const handle = (message: Message): string => {\n if (message.type === 'open') return 'opened';\n switch (message.type) { case 'close': return 'closed'; default: return 'other'; }\n};\n";
    for (argument, expected) in [
        ("{ type: 'close', text: 'value' }", 0),
        ("{ type: 'open' }", 1),
        // 削除した判別値は残っていないが、残る要素の型に合わない。
        ("{ type: 'close', text: 123 }", 1),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write(
            "src/messages.ts",
            "export type Message = { type: 'open' } | { type: 'close'; text: string };\n",
        );
        repo.write("src/handler.ts", before);
        repo.write(
            "src/sender.ts",
            format!(
                "import {{ handle }} from './handler';\nexport const result = handle({argument});\n"
            ),
        );
        repo.commit_all("initial");
        repo.write(
            "src/messages.ts",
            "export type Message = { type: 'close'; text: string };\n",
        );
        repo.write(
            "src/handler.ts",
            before.replace(" if (message.type === 'open') return 'opened';\n", ""),
        );
        let (code, json) = union_hook(&repo, false, false);
        assert_eq!(code, expected, "{argument}: {json}");
        let bucket = if expected == 0 { "mod_closed" } else { "mod" };
        assert!(
            json["api"][bucket]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == "Message")),
            "{argument}: {json}"
        );
    }
}

/// 要素の削除でも、`const` に代入した矢印関数を受け口として扱う。
#[test]
fn literal_union_removal_accepts_arrow_readers() {
    let mode = "export type Mode = \"all\" | \"legacy\" | \"recent\";\nexport const matches = (mode: Mode, age: number): boolean => {\n    switch (mode) {\n        case \"all\": return true;\n        case \"legacy\": return age > 30;\n        case \"recent\": return age <= 7;\n    }\n};\n";
    let view = "import { matches } from \"./mode\";\nexport function visible(age: number): boolean {\n    return matches(\"all\", age);\n}\n";
    // 2 つ目は削除した値が残る形、3 つ目は残っていないが値の由来が分からない形。
    for (argument, expected) in [
        ("\"all\"", 0),
        ("\"legacy\"", 1),
        ("JSON.parse(\"\\\"all\\\"\")", 1),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write("src/mode.ts", mode);
        repo.write("src/view.ts", view);
        repo.commit_all("initial");
        repo.write(
            "src/mode.ts",
            mode.replace(" | \"legacy\"", "")
                .replace("        case \"legacy\": return age > 30;\n", ""),
        );
        repo.write(
            "src/view.ts",
            view.replace(
                "matches(\"all\", age)",
                &format!("matches({argument}, age)"),
            ),
        );
        let (code, json) = union_hook(&repo, false, false);
        union_category(code, &json, expected, expected == 0, argument);
    }
}

#[test]
fn literal_union_addition_requires_complete_static_record_keys() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/mode.ts", BEFORE_EXPANSION);
    let write_labels = |record: &str| {
        repo.write(
            "src/labels.tsx",
            format!(
                "import type {{ Mode }} from './mode';\n{record}\nexport function describe(mode: Mode): string {{ return labels[mode]; }}\n"
            ),
        );
    };
    write_labels("const labels: Record<Mode, string> = { early: 'Early', seen: 'Seen' };");
    repo.commit_all("initial");
    expand_modes(&repo);
    for (case, record, expected) in [
        (
            "annotated record",
            "const labels: Record<Mode, string> = { early: 'Early', seen: 'Seen', filtered: 'Filtered' };",
            0,
        ),
        (
            "satisfies record",
            "const labels = { early: 'Early', seen: 'Seen', 'filtered': 'Filtered' } satisfies Record<Mode, string>;",
            0,
        ),
        (
            "missing record key",
            "const labels = { early: 'Early', seen: 'Seen' } satisfies Record<Mode, string>;",
            1,
        ),
        (
            "partial record",
            "const labels = { early: 'Early', seen: 'Seen', filtered: 'Filtered' } satisfies Partial<Record<Mode, string>>;",
            1,
        ),
        (
            "computed key",
            "const labels = { early: 'Early', seen: 'Seen', ['filtered']: 'Filtered' } satisfies Record<Mode, string>;",
            1,
        ),
        (
            "spread",
            "const labels = { early: 'Early', seen: 'Seen', ...{ filtered: 'Filtered' } } satisfies Record<Mode, string>;",
            1,
        ),
    ] {
        write_labels(record);
        let (code, json) = union_hook(&repo, false, false);
        union_category(code, &json, expected, expected == 0, case);
    }
}

const OLD_MODE: &str = r#"export type Mode = "all" | "legacy" | "recent";
export const MODE_LABELS: Record<Mode, string> = {
    all: "All",
    legacy: "Legacy",
    recent: "Recent",
};
export function matches(mode: Mode, age: number): boolean {
    switch (mode) {
        case "all": return true;
        case "legacy": return age > 30;
        case "recent": return age <= 7;
    }
}
"#;
const OLD_VIEW: &str = r#"import { type Mode, MODE_LABELS, matches } from "./mode";
const MODES: Mode[] = ["all", "legacy", "recent"];
export function visible(age: number): string[] {
    return MODES.filter((m) => matches(m, age)).map((m) => MODE_LABELS[m]);
}
"#;

fn union_fixture(extension: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let text = |source: &str| {
        if extension == "tsx" {
            format!("// 区分の表示\r\n{}", source.replace('\n', "\r\n"))
        } else {
            source.to_string()
        }
    };
    let mode = text(OLD_MODE);
    let view = text(OLD_VIEW);
    repo.write(format!("src/mode.{extension}"), &mode);
    repo.write(format!("src/view.{extension}"), &view);
    repo.commit_all("initial");
    repo.write(
        format!("src/mode.{extension}"),
        mode.replace(" | \"legacy\"", "")
            .replace("    legacy: \"Legacy\",\r\n", "")
            .replace("    legacy: \"Legacy\",\n", "")
            .replace("        case \"legacy\": return age > 30;\r\n", "")
            .replace("        case \"legacy\": return age > 30;\n", ""),
    );
    repo.write(
        format!("src/view.{extension}"),
        view.replace("\"all\", \"legacy\", \"recent\"", "\"all\", \"recent\""),
    );
    repo
}

fn union_hook(repo: &TestRepo, staged: bool, strict: bool) -> (i32, serde_json::Value) {
    let mut cmd = cargo_bin();
    cmd.args(["review", "--git", "--hook", "--format", "json", "--dir"])
        .arg(repo.root());
    if staged {
        cmd.arg("--staged");
    }
    if strict {
        cmd.arg("--strict-public-const-values");
    }
    let output = cmd.output().unwrap();
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stderr).unwrap(),
    )
}

fn union_category(code: i32, json: &serde_json::Value, expected: i32, closed: bool, case: &str) {
    assert_eq!(code, expected, "{case}: {json}");
    let bucket = if closed { "mod_closed" } else { "mod" };
    assert!(
        json["api"][bucket]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|e| e["n"] == "Mode")),
        "{case}: {json}"
    );
    if !closed {
        assert!(
            json["blocking_categories"]
                .as_array()
                .is_some_and(|categories| categories.iter().any(|category| category == "api.mod")),
            "{case}: {json}"
        );
        assert!(
            !json["api"]["mod_closed"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == "Mode")),
            "{case}: {json}"
        );
    }
}

fn union_rewrite(repo: &TestRepo, path: &str, old: &str, new: &str) {
    let source = std::fs::read_to_string(repo.root().join(path)).unwrap();
    assert!(source.contains(old), "missing fixture {old}");
    repo.write(path, source.replace(old, new));
}

#[test]
fn literal_union_removal_closes_record_switch_and_array() {
    for extension in ["ts", "tsx"] {
        let repo = union_fixture(extension);
        let refs = repo.run_json("refs", &["--name", "Mode"]);
        assert_eq!(refs["refs"].as_array().unwrap().len(), 5, "{refs}");
        assert_eq!(
            refs["refs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["kind"] == "def")
                .count(),
            1,
            "{refs}"
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 0, true, extension);
        assert!(
            hook["api"]["const_value"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["n"] == "MODE_LABELS"),
            "{hook}"
        );
        let normal = repo.run_json("review", &["--git"]);
        assert!(
            normal["api_changes"]["modified_closed_in_diff"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "Mode"),
            "{normal}"
        );
        let (code, strict) = union_hook(&repo, false, true);
        union_category(code, &strict, 1, true, "strict const remains blocking");
        repo.stage_all();
        repo.write(
            format!("src/mode.{extension}"),
            "export const worktreeOnly = 1;\n",
        );
        let (code, staged) = union_hook(&repo, true, false);
        union_category(code, &staged, 0, true, "index snapshot");
    }
}

#[test]
fn literal_union_removal_requires_each_removed_use_to_follow() {
    for (path, old, new) in [
        (
            "src/mode.ts",
            "    recent:",
            "    legacy: 'Legacy',\n    recent:",
        ),
        (
            "src/mode.ts",
            "        case \"recent\":",
            "        case 'legacy': return age > 30;\n        case \"recent\":",
        ),
        (
            "src/view.ts",
            "[\"all\", \"recent\"]",
            "[\"all\", \"legacy\", \"recent\"]",
        ),
    ] {
        let repo = union_fixture("ts");
        union_rewrite(&repo, path, old, new);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, new);
    }
}

#[test]
fn literal_union_removal_audits_exact_cooked_residue() {
    for (path, residue) in [
        ("src/extra.ts", "const dict = { legacy: 1 };"),
        ("src/extra.ts", "type X = 'legacy';"),
        ("src/extra.ts", "const text = `legacy`;"),
        ("src/extra.ts", "const text = 'leg\\u0061cy';"),
        (
            "src/extra.ts",
            "const text = '\\u006c\\u0065\\u0067\\u0061\\u0063\\u0079';",
        ),
        ("src/extra.ts", "const dict = { leg\\u0061cy: 1 };"),
        ("src/extra.ts", "const text = item.legacy;"),
        ("src/extra.js", "/** @type {'legacy'} */ let value;"),
        ("src/extra.vue", "<script>const x = 'legacy';</script>"),
        ("src/extra.json", "{\"legacy\": 1}"),
        ("src/extra.json", "{\"value\": \"leg\\u0061cy\"}"),
    ] {
        let repo = union_fixture("ts");
        repo.write(path, residue);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, residue);
    }
    let repo = union_fixture("ts");
    repo.write("src/extra.ts", "const base = 'legacy-peer'; const path = '\\n'; const unrelated = `${base}/${path}`; type OtherMode = 'other';\n");
    let (code, hook) = union_hook(&repo, false, false);
    union_category(
        code,
        &hook,
        0,
        true,
        "unrelated template and escaped newline",
    );
}

#[test]
fn literal_union_removal_keeps_queries_aliases_and_unknown_flows_blocking() {
    for extra in [
        "type X = typeof MODES[number];",
        "type X = keyof typeof MODE_LABELS;",
        "type X = Parameters<typeof matches>;",
        "const alias = MODES;",
        "const alias = MODE_LABELS;",
        "const alias = matches;",
        "MODES.push('all');",
        "MODE_LABELS['all'] = 'changed';",
        "matches(`leg${'acy'}` as never, 1);",
        "matches(externalMode, 1);",
        "matches('all' as Mode, 1);",
        "const value: Mode = 'all';",
        "const value = toExternal(MODES);",
        "const value = toExternal(MODE_LABELS);",
    ] {
        let repo = union_fixture("ts");
        let path = if extra.contains("MODE_LABELS") || extra.contains("matches") {
            "src/mode.ts"
        } else {
            "src/view.ts"
        };
        let mut text = std::fs::read_to_string(repo.root().join(path)).unwrap();
        text.push_str(extra);
        repo.write(path, text);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, extra);
    }
    for (path, old, new) in [
        (
            "src/view.ts",
            "MODES.filter((m) => matches(m, age))",
            "MODES.filter(matches)",
        ),
        (
            "src/view.ts",
            ".map((m) => MODE_LABELS[m])",
            ".map((m) => m)",
        ),
        (
            "src/view.ts",
            ".map((m) => MODE_LABELS[m])",
            ".map((m) => external(m))",
        ),
        ("src/mode.ts", "switch (mode)", "switch (mode = 'all')"),
        (
            "src/mode.ts",
            "switch (mode)",
            "var mode; mode = 'recent'; switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "({mode} = externalObject); switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "for ({mode} of externalObjects) {} switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "for (MODE_LABELS[mode] of values) {} switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "({x: MODE_LABELS[mode]} = externalObject); switch (mode)",
        ),
        (
            "src/mode.ts",
            "switch (mode)",
            "if (mode === LIB.X) return false; switch (mode)",
        ),
    ] {
        let repo = union_fixture("ts");
        union_rewrite(&repo, path, old, new);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, new);
    }
    let repo = union_fixture("ts");
    repo.write(
        "src/other.ts",
        "export type Mode = 'other'; export const value: Mode = 'other';\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 0, true, "independent same-name type binding");
    let repo = union_fixture("ts");
    union_rewrite(&repo, "src/view.ts", "type Mode,", "type Mode as M,");
    union_rewrite(&repo, "src/view.ts", "MODES: Mode[]", "MODES: M[]");
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 1, false, "type import alias");
    for extra in [
        "export type { Mode } from './mode';",
        "import * as modes from './mode';",
        "declare const Record: unknown;",
        "Array.prototype.map = replacement;",
    ] {
        let repo = union_fixture("ts");
        repo.write("src/extra.ts", extra);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, extra);
    }
}

#[test]
fn literal_union_removal_uses_index_for_negative_snapshot() {
    let repo = union_fixture("ts");
    union_rewrite(
        &repo,
        "src/view.ts",
        "[\"all\", \"recent\"]",
        "[\"all\", \"legacy\", \"recent\"]",
    );
    repo.stage_all();
    union_rewrite(
        &repo,
        "src/view.ts",
        "[\"all\", \"legacy\", \"recent\"]",
        "[\"all\", \"recent\"]",
    );
    let (code, staged) = union_hook(&repo, true, false);
    union_category(code, &staged, 1, false, "index contains removed element");
    let (code, working) = union_hook(&repo, false, false);
    union_category(code, &working, 0, true, "working tree fixed");
}

#[test]
fn literal_union_removal_keeps_unsupported_alias_changes_blocking() {
    for declaration in [
        "export type Mode = \"recent\" | \"all\";",
        "export type Mode = \"all\" | \"new\";",
        "export type Mode<T> = \"all\" | \"recent\";",
        "type Other = \"all\"; export type Mode = Other | \"recent\";",
        "export type Mode = \"all\" | 1;",
        "export type Mode = \"all\" | \"r\\u0065cent\";",
        "export type Mode = \"all\" | \"all\" | \"recent\";",
        "export type Mode = \"all\" | \"recent\"; const Mode = 1;",
    ] {
        let repo = union_fixture("ts");
        union_rewrite(
            &repo,
            "src/mode.ts",
            "export type Mode = \"all\" | \"recent\";",
            declaration,
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, declaration);
    }
}

#[test]
fn literal_union_removal_checks_array_flow_and_ignored_residue_scope() {
    for (old, new) in [
        ("MODES: Mode[]", "MODES: readonly Mode[]"),
        ("MODES.filter((m) => matches(m, age)).map", "MODES.map"),
        (
            ".map((m) => MODE_LABELS[m])",
            ".filter((m) => m !== 'all').map((m) => MODE_LABELS[m])",
        ),
    ] {
        let repo = union_fixture("ts");
        union_rewrite(&repo, "src/view.ts", old, new);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 0, true, new);
    }
    let repo = union_fixture("ts");
    repo.write(".gitignore", "src/ignored.json\n");
    repo.write("src/ignored.json", "{\"legacy\": 1}");
    repo.write(
        "src/extra.ts",
        "const isArray = Array.isArray('unrelated');\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(
        code,
        &hook,
        0,
        true,
        "ignored JSON and ordinary builtin read",
    );
}

#[test]
fn literal_union_removal_keeps_dynamic_calls_and_resolution_barriers_blocking() {
    for (path, source) in [
        ("src/extra.ts", "eval('0');"),
        ("src/extra.ts", r"\u0065val('0');"),
        ("src/extra.ts", "new Function('return 0')();"),
        ("src/extra.ts", "const loaded = module.require('./mode');"),
        (
            "src/extra.ts",
            "const loaded = require.context('.', false, /mode/);",
        ),
        ("src/extra.ts", "const loaded = import.meta.glob('./*.ts');"),
        (
            "src/extra.ts",
            "import { createRequire as load } from 'node:module'; const loader = load(import.meta.url);",
        ),
        (
            "src/extra.ts",
            r"import { createRequ\u0069re as load } from 'node:module'; const loader = load(import.meta.url);",
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"rootDirs":["src","other"]}}"#,
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"root\u0044irs":["src","other"]}}"#,
        ),
        ("tsconfig.json", r#"{"extends":"./missing.json"}"#),
    ] {
        let repo = union_fixture("ts");
        repo.write(path, source);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, source);
    }
    for usage in [
        "MODES.filter?.((m) => matches(m, age)).map((m) => MODE_LABELS[m])",
        "MODES.filter((m) => matches(m, age)).map?.((m) => MODE_LABELS[m])",
        "MODES.filter((m) => matches(m, age)).map<string>((m) => MODE_LABELS[m])",
        "MODES.filter((m) => matches(m, age)).map((m) => MODE_LABELS[m], undefined)",
    ] {
        let repo = union_fixture("ts");
        union_rewrite(
            &repo,
            "src/view.ts",
            "MODES.filter((m) => matches(m, age)).map((m) => MODE_LABELS[m])",
            usage,
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, usage);
    }
    for transport in [
        "import * as hidden from '#mode';",
        "export * from '#mode';",
        "import hidden from 'unknown-package';",
    ] {
        let repo = union_fixture("ts");
        repo.write(
            "package.json",
            r##"{"name":"demo","imports":{"#mode":"./src/mode.ts"}}"##,
        );
        repo.write("src/extra.ts", transport);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, transport);
    }
    let repo = union_fixture("ts");
    repo.write("src/other.ts", "export const count = 1;\n");
    repo.write(
        "src/extra.ts",
        "import * as other from './other';\nimport { \"count\" as c } from './other';\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 0, true, "transport of proven different module");
}

fn compact_union_fixture(name: &str, old_extra: &str, new_extra: &str, argument: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    // 型参照を置いた全ての行を編集し、行ベース判定への復帰を検出する。
    let mode = format!(
        r#"export type {name} = "all" | "legacy" | "recent";
export const LABELS: Record<{name}, string> = {{ all: "All", legacy: "Legacy", recent: "Recent" }};
export function matches(mode: {name}, age: number): boolean {{ switch (mode) {{ case "all": return true; case "legacy": return age > 30; case "recent": return age <= 7; }} }}
"#
    );
    let view = format!(
        r#"import {{ type {name}, LABELS, matches }} from "./mode";
const MODES: {name}[] = ["all", "legacy", "recent"];
export function visible(age: number): string[] {{ return MODES.filter(m => matches(m, age)).map(m => LABELS[m]); }}
"#
    );
    repo.write("src/mode.tsx", format!("{mode}{old_extra}"));
    repo.write("src/view.tsx", &view);
    repo.commit_all("initial");
    repo.write(
        "src/mode.tsx",
        format!(
            "{}{new_extra}",
            mode.replace(" | \"legacy\"", "")
                .replace("legacy: \"Legacy\", ", "")
                .replace("case \"legacy\": return age > 30; ", "")
        ),
    );
    repo.write(
        "src/view.tsx",
        view.replace(", \"legacy\"", "")
            .replace("matches(m, age)", &format!("matches({argument}, age)")),
    );
    repo
}

#[test]
fn literal_union_removal_does_not_fall_back_for_unsupported_target_names() {
    for name in ["方式", r"Mo\u0064e"] {
        let repo = compact_union_fixture(name, "", "", "m");
        let (code, hook) = union_hook(&repo, false, false);
        assert_eq!(code, 1, "{name}: {hook}");
        assert!(
            hook["api"]["mod"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == name)),
            "{name}: {hook}"
        );
    }
}

#[test]
fn literal_union_removal_keeps_recognized_candidates_blocking_on_new_parse_limits() {
    let deep = format!(
        "\nconst deep = {}<span/>{};\n",
        "<div>".repeat(140),
        "</div>".repeat(140)
    );
    let broken = "\nconst broken = ;\n";
    for (old_extra, new_extra) in [
        ("", ""),
        ("", broken),
        ("", deep.as_str()),
        (broken, broken),
        (deep.as_str(), deep.as_str()),
    ] {
        let repo = compact_union_fixture(
            "Mode",
            old_extra,
            new_extra,
            "(globalThis as any).raw as never",
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(
            code,
            &hook,
            1,
            false,
            "unknown argument stays blocking despite new parse failure/limit",
        );
    }
}

#[test]
fn literal_union_removal_audits_embedded_and_import_require_transports() {
    for source in [
        r#"<script setup lang="ts">import { matches } from './mode'; const ok = matches(props.mode as never, 1);</script>"#,
        r#"<script setup lang="ts">import { type Mode } from './mode'; const props = defineProps<{mode: Mode}>();</script>"#,
        r#"<script setup lang="ts">import * as hidden from '#alias'; const exported = hidden;</script>"#,
    ] {
        let repo = union_fixture("ts");
        repo.write("src/Picker.vue", source);
        repo.write(
            "package.json",
            r##"{"imports":{"#alias":"./src/mode.ts"}}"##,
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, source);
    }
    for source in [
        "import m = require('./mode'); m['matches'](external as never, 1);",
        "export import m = require('./mode');",
        "import { \"matches\" as check } from './mode'; check((globalThis as any).raw as never, 1);",
        "import { \"MODE_LABELS\" as labels } from './mode'; const label = labels['all'];",
    ] {
        let repo = union_fixture("ts");
        repo.write("src/extra.ts", source);
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, source);
    }
    let repo = union_fixture("ts");
    let mode = std::fs::read_to_string(repo.root().join("src/mode.ts")).unwrap();
    repo.write("src/mode.ts", format!("{mode}\nexport default matches;\n"));
    repo.write(
        "src/extra.ts",
        "import { default as check } from './mode'; const ok = check('all', 1);\n",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(
        code,
        &hook,
        1,
        false,
        "default export/import alias stays blocking",
    );
    let repo = union_fixture("ts");
    let output = cargo_bin()
        .current_dir(repo.root())
        .args([
            "review", "--dir", ".", "--git", "--hook", "--format", "json",
        ])
        .output()
        .unwrap();
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    union_category(
        output.status.code().unwrap(),
        &hook,
        0,
        true,
        "relative analysis root",
    );
}

#[test]
fn literal_union_removal_rejects_escaped_binding_writes_and_shadowing() {
    for prefix in [
        r"m\u006fde = (globalThis as any).raw;",
        r"{ let m\u006fde = (globalThis as any).raw; return MODE_LABELS[mode] !== ''; }",
        "function mode() {}",
    ] {
        let repo = union_fixture("ts");
        union_rewrite(
            &repo,
            "src/mode.ts",
            "switch (mode)",
            &format!("{prefix} switch (mode)"),
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, prefix);
    }
    let repo = union_fixture("ts");
    union_rewrite(
        &repo,
        "src/view.ts",
        "matches(m, age)",
        r"matches(m, (\u006d = (globalThis as any).raw, age))",
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 1, false, "escaped callback parameter write");
}

#[test]
fn literal_union_removal_preserves_other_alias_classifications() {
    for (name, old_type, new_type, old_value, new_value) in [
        ("Flag", "true", "false", "true", "false"),
        ("Id", "string", "number", "\"a\"", "1"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let old = format!(
            "export type {name} = {old_type};\nexport function consume(value: {name}): string {{ return String(value) + 'old'; }}\n"
        );
        let new = format!(
            "export type {name} = {new_type};\nexport function consume(value: {name}): string {{ return String(value) + 'new'; }}\n"
        );
        let view = format!(
            "import {{ type {name}, consume }} from './types';\nconst input: {name} = {old_value};\nexport const output = consume(input);\n"
        );
        repo.write("types.ts", old);
        repo.write("view.ts", &view);
        repo.commit_all("initial");
        repo.write("types.ts", new);
        repo.write(
            "view.ts",
            view.replace(&format!("= {old_value};"), &format!("= {new_value};")),
        );
        let (code, hook) = union_hook(&repo, false, false);
        assert_eq!(code, 0, "{name}: {hook}");
        assert!(
            hook["api"]["mod_closed"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["n"] == name)),
            "{name}: {hook}"
        );
    }
}

#[test]
fn literal_union_removal_audits_ignored_resolution_inputs() {
    for case in 0..4 {
        let package_entry = case == 1;
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src/choice");
        let (target, other, spec, direct, metadata) = if package_entry {
            (
                "src/choice/target.ts",
                "src/choice/index.ts",
                "./choice",
                "./choice/target",
                "src/choice/package.json",
            )
        } else {
            (
                "src/mode.native.ts",
                "src/mode.ts",
                "./mode",
                "./mode.native",
                match case {
                    2 => "native/tsconfig.json",
                    3 => ".native/tsconfig.json",
                    _ => "src/tsconfig.json",
                },
            )
        };
        repo.write(target, OLD_MODE);
        repo.write(other, "export type Mode = string; export const MODE_LABELS: Record<Mode, string> = {}; export function matches(mode: Mode, age: number): boolean { return false; }\n");
        let view = OLD_VIEW.replace("./mode", spec);
        repo.write("src/view.ts", &view);
        repo.write(
            "src/safe.ts",
            format!("import {{ matches }} from '{direct}'; export const ok = matches('all', 1);\n"),
        );
        if case >= 2 {
            let dir = if case == 2 { "native" } else { ".native" };
            repo.create_dir_all(dir);
            repo.write(
                "tsconfig.json",
                format!(r#"{{"files":[],"references":[{{"path":"./{dir}"}}]}}"#),
            );
        }
        repo.write(".gitignore", format!("{metadata}\n"));
        repo.write(
            metadata,
            if package_entry {
                r#"{"types":"./target.ts"}"#
            } else {
                r#"{"compilerOptions":{"moduleSuffixes":[".native",""]}}"#
            },
        );
        repo.commit_all("initial");
        repo.write(
            target,
            OLD_MODE
                .replace(" | \"legacy\"", "")
                .replace("    legacy: \"Legacy\",\n", "")
                .replace("        case \"legacy\": return age > 30;\n", ""),
        );
        repo.write(
            "src/view.ts",
            view.replace(", \"legacy\"", "").replace(
                "matches(m, age)",
                "matches((globalThis as any).raw as never, age)",
            ),
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, metadata);
    }
}

#[test]
fn literal_union_removal_accepts_ignored_plain_ancestor_config() {
    let repo = union_fixture("ts");
    repo.write(".gitignore", "src/tsconfig.json\n");
    repo.write(
        "src/tsconfig.json",
        r#"{"compilerOptions":{"strict":true}}"#,
    );
    let (code, hook) = union_hook(&repo, false, false);
    union_category(code, &hook, 0, true, "ignored plain ancestor config");
}

#[test]
fn literal_union_removal_keeps_declaration_resolution_ambiguity_blocking() {
    for (declaration, other, spec) in [
        ("mode.d.ts", "mode/index.ts", "./mode"),
        ("mode.d.ts", "mode.js", "./mode"),
        ("mode.d.ts", "mode.js", "./mode.js"),
        ("mode.d.mts", "mode.mjs", "./mode.mjs"),
        ("mode.d.cts", "mode.cjs", "./mode.cjs"),
        ("mode.d.css.ts", "mode.css.ts", "./mode.css"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("package.json", r#"{"type":"module"}"#);
        repo.create_dir_all("mode");
        if declaration == "mode.d.css.ts" {
            repo.write(
                "tsconfig.json",
                r#"{"compilerOptions":{"allowArbitraryExtensions":true}}"#,
            );
        }
        repo.write(
            declaration,
            "export type Mode = 'all' | 'legacy' | 'recent';\n",
        );
        repo.write(
            other,
            if other.ends_with(".ts") {
                "export type Mode = string;\n"
            } else {
                "export class Mode {}\n"
            },
        );
        repo.write("safe.ts", format!("import type {{ Mode }} from './{declaration}';\nfunction check(mode: Mode): boolean {{ return mode === 'all'; }}\nexport const ok = check('all');\n"));
        repo.write("view.ts", format!("import {{ type Mode }} from '{spec}';\nexport const input: Mode = (globalThis as any).raw;\n"));
        repo.commit_all("initial");
        repo.write(declaration, "export type Mode = 'all' | 'recent';\n");
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, spec);
    }
}

#[test]
fn literal_union_removal_checks_implementation_before_explicit_declaration_import() {
    for (implementation, declaration) in [
        ("mode.ts", "mode.d.ts"),
        ("mode.mts", "mode.d.mts"),
        ("mode.cts", "mode.d.cts"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("package.json", r#"{"type":"module"}"#);
        let old = format!("{OLD_MODE}\nexport const ok = matches('all', 1);\n");
        repo.write(implementation, &old);
        repo.write(declaration, "export type Mode = string;\n");
        repo.write("view.ts", format!("import type {{ Mode }} from './{declaration}';\nexport const input: Mode = (globalThis as any).raw;\n"));
        repo.commit_all("initial");
        repo.write(
            implementation,
            old.replace(" | \"legacy\"", "")
                .replace("    legacy: \"Legacy\",\n", "")
                .replace("        case \"legacy\": return age > 30;\n", ""),
        );
        let (code, hook) = union_hook(&repo, false, false);
        union_category(code, &hook, 1, false, declaration);
    }
}

#[test]
fn literal_union_removal_checks_legacy_javascript_resolution_inputs() {
    for (module, resolution) in [("commonjs", "node10"), ("esnext", "classic")] {
        for directory_package in [false, true] {
            if directory_package && resolution == "classic" {
                continue;
            }
            let repo = TestRepo::new();
            repo.init_git();
            repo.create_dir_all("mode");
            let declaration = if directory_package {
                "mode/types.d.ts"
            } else {
                "mode.js.d.ts"
            };
            repo.write("tsconfig.json", format!(r#"{{"compilerOptions":{{"module":"{module}","moduleResolution":"{resolution}","strict":true}}}}"#));
            repo.write(
                declaration,
                "export type Mode = 'all' | 'legacy' | 'recent';\n",
            );
            repo.write("mode.js", "export class Mode {}\n");
            if directory_package {
                repo.write("mode/package.json", r#"{"types":"./types.d.ts"}"#);
            }
            repo.write("safe.ts", format!("import type {{ Mode }} from './{declaration}';\nfunction check(mode: Mode): boolean {{ return mode === 'all'; }}\nexport const ok = check('all');\n"));
            let specifier = if directory_package {
                "./mode"
            } else {
                "./mode.js"
            };
            repo.write("view.ts", format!("import type {{ Mode }} from '{specifier}';\nexport const input: Mode = (globalThis as any).raw;\n"));
            repo.commit_all("initial");
            repo.write(declaration, "export type Mode = 'all' | 'recent';\n");
            let (code, hook) = union_hook(&repo, false, false);
            union_category(
                code,
                &hook,
                1,
                false,
                &format!("{resolution}/{declaration}"),
            );
        }
    }
}
