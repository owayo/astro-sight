//! TS/TSX の名前空間内の同名 export が直下 API の変更を隠さない (Issue #144)。

use super::support::{TestRepo, cargo_bin};

fn hook(repo: &TestRepo, staged: bool) -> (i32, serde_json::Value) {
    let mut cmd = cargo_bin();
    cmd.args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        cmd.arg("--staged");
    }
    let output = cmd.output().unwrap();
    let json = if output.stderr.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&output.stderr).unwrap()
    };
    (output.status.code().unwrap(), json)
}

fn has_name(json: &serde_json::Value, category: &str, name: &str) -> bool {
    json["api"][category]
        .as_array()
        .is_some_and(|changes| changes.iter().any(|change| change["n"] == name))
}

#[test]
fn namespace_collision_required_argument_stays_blocking_in_worktree_and_index() {
    for (extension, namespace_first, hidden_index) in [
        ("ts", false, false),
        ("tsx", true, false),
        ("ts", false, true),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let path = format!("api.{extension}");
        let root = "export function run(value: string): void {}\n";
        let nested = if hidden_index {
            "namespace N { export function run(value: string, required: boolean): void {} }\n"
        } else {
            "export namespace N { export function run(value: number): void {} }\n"
        };
        let local = if hidden_index {
            "export function local(): void { run('local'); }\n"
        } else {
            ""
        };
        let before = if namespace_first {
            format!("{nested}{root}{local}")
        } else {
            format!("{root}{nested}{local}")
        };
        repo.write(&path, &before);
        repo.write("use.ts", "import { run } from './api';\nexport function main(): void {\n    run('x');\n}\nmain();\n");
        repo.commit_all("initial");
        let occurrences = |json: serde_json::Value| -> Vec<_> {
            json["refs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    (
                        r["path"].clone(),
                        r["ln"].clone(),
                        r["col"].clone(),
                        r["kind"].clone(),
                    )
                })
                .collect()
        };
        let refs = occurrences(repo.run_json("refs", &["--name", "run"]));
        repo.write(
            &path,
            before
                .replace(
                    root,
                    "export function run(value: string, required: boolean): void {}\n",
                )
                .replace("run('local')", "run( 'local' )"),
        );
        repo.write("use.ts", "import { run } from './api';\nexport function main(): void {\n    run( 'x' );\n}\nmain();\n");
        assert_eq!(occurrences(repo.run_json("refs", &["--name", "run"])), refs);
        let normal = repo.run_json("review", &["--git"]);
        let modified = normal["api_changes"]["modified"].as_array().unwrap();
        assert_eq!(
            modified
                .iter()
                .filter(|change| change["name"] == "run")
                .count(),
            1,
            "{normal}"
        );
        let (code, json) = hook(&repo, false);
        assert_eq!(code, 1, "{json}");
        assert!(has_name(&json, "mod", "run"), "{json}");
        assert!(!has_name(&json, "mod_closed", "run"), "{json}");
        repo.stage_all();
        repo.write(&path, "export const unrelated = 1;\n");
        repo.write("use.ts", "export const unrelated = 2;\n");
        let (code, staged) = hook(&repo, true);
        assert_eq!(code, 1, "{staged}");
        assert!(has_name(&staged, "mod", "run"), "{staged}");
    }
}

#[test]
fn namespace_collision_type_members_and_variables_keep_existing_audits() {
    for (name, before, after, nested, use_before, use_after) in [
        (
            "Row",
            "export interface Row { id: string }\n",
            "export interface Row { id: string; note: string }\n",
            "namespace N { export interface Row { other: number } }\n",
            "import type { Row } from './api';\nexport const value: Row = { id: 'x' };\n",
            "import type { Row } from './api';\nexport const value: Row = { id: 'x' }; // touched\n",
        ),
        (
            "Row",
            "export type Row = { id: string; note: string | null };\n",
            "export type Row = { id: string; note: string };\n",
            "namespace N { export type Row = { other: number }; }\n",
            "import type { Row } from './api';\nexport const value: Row = { id: 'x', note: null };\n",
            "import type { Row } from './api';\nexport const value: Row = { id: 'x', note: null }; // touched\n",
        ),
        (
            "LIMIT",
            "export const LIMIT: number = 1;\n",
            "export const LIMIT: string = 'x';\n",
            "namespace N { export const LIMIT: boolean = true; }\n",
            "import { LIMIT } from './api';\nexport const doubled = LIMIT * 2;\n",
            "import { LIMIT } from './api';\nexport const doubled = LIMIT  * 2;\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("api.ts", format!("{before}{nested}"));
        repo.write("use.ts", use_before);
        repo.commit_all("initial");
        repo.write("api.ts", format!("{after}{nested}"));
        repo.write("use.ts", use_after);
        let (code, json) = hook(&repo, false);
        assert_eq!(code, 1, "{json}");
        assert!(has_name(&json, "mod", name), "{json}");
        assert!(!has_name(&json, "mod_closed", name), "{json}");
        // namespaceを除いた対照。変数の型変更は既存の行ベース方針のまま。
        repo.write("api.ts", after);
        let (_, control) = hook(&repo, false);
        let category = if name == "LIMIT" { "mod_closed" } else { "mod" };
        assert!(has_name(&control, category, name), "{control}");
    }
}
