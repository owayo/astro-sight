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
