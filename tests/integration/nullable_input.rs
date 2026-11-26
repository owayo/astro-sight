use super::support::{TestRepo, cargo_bin};

const BEFORE: &str = "export function word(text: string): string { return String(text); }\n";
const AFTER: &str = "export function word(text: string | null): string { return String(text); }\n";

fn repo(after: &str, caller: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("render.ts", BEFORE);
    repo.write("call.ts", caller);
    repo.commit_all("initial");
    repo.write("render.ts", after);
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
    let value = serde_json::from_slice(&output.stderr).unwrap();
    (output, value)
}

#[test]
fn parameter_union_widening_review_uses_the_selected_snapshot() {
    let before = r#"export function formatLabel(prefix: string, value: number, options?: { pad: number }) {
    return `${prefix ?? ""}${String(value).padStart(options?.pad ?? 0, "0")}`;
}
"#;
    let caller = "import { formatLabel } from './label.js';\nexport const label = formatLabel('No.', 7, { pad: 3 });\n";
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("label.ts", before);
    repo.write("main.ts", caller);
    repo.commit_all("initial");
    for (member, reason) in [
        ("null", "nullable_input_widening"),
        ("undefined", "parameter_union_widening"),
    ] {
        repo.write(
            "label.ts",
            before.replace("prefix: string", &format!("prefix: string | {member}")),
        );
        repo.stage_all();
        let (out, result) = hook(&repo, false);
        assert!(out.status.success(), "{result}");
        assert!(
            result["impacts"].is_null() && result["api"]["mod"].is_null(),
            "{result}"
        );
        assert_eq!(result["api"]["mod_compat"][0]["reason"], reason);
        let evidence = &result["api"]["type_change_evidence"][0];
        assert_eq!(evidence["usage"], "direct_calls_proven");
        assert_eq!(evidence["changes"][0]["member"], member);
        repo.write(
            "label.ts",
            before.replace("prefix: string", "prefix: number | null"),
        );
        assert_eq!(hook(&repo, false).0.status.code(), Some(1));
        assert!(hook(&repo, true).0.status.success());
    }
}

#[test]
fn nullable_input_review_preserves_evidence_and_staged_scope() {
    let repo = repo(
        AFTER,
        "import { word } from './render';\nconsole.log(word('a'));\n",
    );
    let (output, value) = hook(&repo, false);
    assert!(output.status.success(), "{value}");
    assert!(value["impacts"].is_null(), "{value}");
    assert_eq!(
        value["api"]["mod_compat"][0]["reason"],
        "nullable_input_widening"
    );
    assert_eq!(
        value["api"]["type_change_evidence"][0]["usage"],
        "direct_calls_proven"
    );
    assert_eq!(
        value["api"]["type_change_evidence"][0]["changes"][0]["position"],
        "input"
    );
    let normal = repo.run_json("review", &["--git"]);
    assert_eq!(
        normal["api_changes"]["type_change_evidence"][0]["changes"][0]["direction"],
        "widening"
    );
    let toon = cargo_bin()
        .args(["review", "--git", "--format", "toon", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert!(toon.status.success());
    assert!(
        String::from_utf8(toon.stdout)
            .unwrap()
            .contains("type_change_evidence")
    );

    repo.stage_all();
    repo.write(
        "call.ts",
        "import { word } from './render';\nregister(word);\n",
    );
    let (output, value) = hook(&repo, true);
    assert!(output.status.success(), "index direct calls: {value}");
    assert_eq!(
        value["api"]["type_change_evidence"][0]["usage"],
        "direct_calls_proven"
    );
    let (output, value) = hook(&repo, false);
    assert_eq!(output.status.code(), Some(1), "worktree handoff: {value}");
    assert_eq!(value["api"]["type_change_evidence"][0]["usage"], "unproven");
    repo.stage_all();
    repo.write("call.ts", "import { word } from './render';\nword('a');\n");
    let (output, value) = hook(&repo, true);
    assert_eq!(output.status.code(), Some(1), "index handoff: {value}");
    assert_eq!(value["api"]["type_change_evidence"][0]["usage"], "unproven");
    let (output, value) = hook(&repo, false);
    assert!(output.status.success(), "worktree direct calls: {value}");
    assert_eq!(
        value["api"]["type_change_evidence"][0]["usage"],
        "direct_calls_proven"
    );
}

#[test]
fn nullable_input_review_keeps_expected_errors_in_the_selected_caller() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"strict":true,"jsx":"preserve"}}"#,
    );
    repo.write("render.ts", BEFORE);
    let import = "import { word } from './render';\n";
    let original = "// @ts-expect-error null was rejected\nword(null);\n";
    repo.write("call.tsx", format!("{import}{original}"));
    repo.commit_all("initial");
    repo.write("render.ts", AFTER);
    repo.stage_all();

    for (caller, blocking) in [
        (original, true),
        (
            "const result = <>{word(\n// @ts-expect-error\n\n// null was rejected\nnull)}</>;\n",
            true,
        ),
        (
            "const count = 1; /* @ts-expect-error */\nword(null);\n",
            true,
        ),
        ("/// @ts-expect-error\nword(null);\n", true),
        (
            "// @ts-expect-error\nconst bad: number = 'str';\nword('ok');\n",
            false,
        ),
        (
            "// Docs mention @ts-expect-error\nword('@ts-expect-error');\n",
            false,
        ),
        ("// @ts-ignore\nword(null);\n", false),
        ("word(null);\n", false),
    ] {
        repo.write("call.tsx", format!("{import}{caller}"));
        let (out, result) = hook(&repo, false);
        assert_eq!(
            out.status.code(),
            Some(i32::from(blocking)),
            "{caller}: {result}"
        );
        if blocking {
            assert_eq!(result["api"]["mod"][0]["n"], "word", "{result}");
            assert_eq!(
                result["api"]["type_change_evidence"][0]["usage"],
                "unproven"
            );
        } else {
            assert_eq!(
                result["api"]["mod_compat"][0]["reason"],
                "nullable_input_widening"
            );
        }
    }
    let (out, result) = hook(&repo, true);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert_eq!(result["api"]["mod"][0]["n"], "word");
}

#[test]
fn nullable_input_keeps_expected_errors_on_edited_import_calls() {
    for (import, call) in [
        ("{ word as render }", "render(null)"),
        ("* as api", "api.word(null)"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("render.ts", BEFORE);
        let prefix = format!("import {import} from './render';\n// @ts-expect-error\n");
        repo.write("use.ts", format!("{prefix}{call};\n"));
        repo.commit_all("initial");
        repo.write("render.ts", AFTER);
        repo.write("use.ts", format!("{prefix}export const result = {call};\n"));
        let (out, result) = hook(&repo, false);
        assert_eq!(out.status.code(), Some(1), "{call}: {result}");
        assert_eq!(result["api"]["mod"][0]["n"], "word", "{result}");
    }
}

#[test]
fn nullable_input_keeps_expected_errors_on_edited_jsx_elements() {
    let repo = TestRepo::new();
    repo.init_git();
    let before =
        "export function Word(props: { text: string }): string { return String(props.text); }\n";
    let prefix = "import { Word } from './render';\n// @ts-expect-error null prop\n";
    repo.write("render.ts", before);
    repo.write(
        "call.tsx",
        format!("{prefix}const view = <Word text={{null}}/>;\n"),
    );
    repo.commit_all("initial");
    repo.write(
        "render.ts",
        before.replace("text: string", "text: string | null"),
    );
    repo.write(
        "call.tsx",
        format!("{prefix}export const view = <Word text={{null}}/>;\n"),
    );
    let (out, result) = hook(&repo, false);
    assert_eq!(out.status.code(), Some(1), "{result}");
    assert_eq!(result["api"]["mod"][0]["n"], "Word", "{result}");
}

#[test]
fn nullable_input_review_keeps_return_and_type_extraction_blocking() {
    for (after, caller, position) in [
        (
            "export function word(text: string): string | null { return String(text); }\n",
            "import { word } from './render';\nword('a');\n",
            "return",
        ),
        (
            AFTER,
            "import { word } from './render';\ntype Props = ComponentProps<typeof word>;\nword('a');\n",
            "input",
        ),
    ] {
        let repo = repo(after, caller);
        let (output, value) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{value}");
        assert_eq!(value["api"]["mod"][0]["n"], "word");
        assert_eq!(
            value["api"]["type_change_evidence"][0]["changes"][0]["position"],
            position
        );
        assert_eq!(value["api"]["type_change_evidence"][0]["usage"], "unproven");
    }
}
