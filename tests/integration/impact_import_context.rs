use super::support::*;

#[test]
fn impact_ts_rebound_const_follows_worktree_and_index() {
    let repo = TestRepo::new();
    let producer = "export function retries(): number {\n  return 3;\n}\n";
    let user = "const SERVER = 'http://old';\nexport const PATTERN = `${SERVER}/*`;\n";
    repo.write("config.ts", producer);
    repo.write("build.ts", user);
    repo.init_git();
    repo.commit_all("initial");
    let changed =
        producer.replace("return 3", "return 5") + "export const SERVER = 'https://new';\n";
    let imported = user.replace(
        "const SERVER = 'http://old';",
        "import { SERVER } from './config';",
    );
    repo.write("config.ts", &changed);
    repo.write("build.ts", &imported);
    for staged in [false, true] {
        if staged {
            repo.stage_all();
            repo.write("config.ts", producer);
            repo.write("build.ts", user);
        }
        let hook = review_hook(&repo, staged, 0);
        assert_eq!(hook["blocking_categories"], serde_json::json!([]), "{hook}");
    }
}

#[test]
fn impact_ts_rebound_const_requires_new_export_matching_type_and_exact_import() {
    let repo = TestRepo::new();
    let config = "export function retry() {\n return 3;\n}\n";
    let reader = "const SERVER = 'old';\nexport const base = `${SERVER}`;\n";
    repo.write("config.ts", config);
    repo.write("reader.ts", reader);
    repo.write(
        "unchanged.ts",
        "import { SERVER } from './config';\nconsole.log(SERVER);\n",
    );
    repo.create_dir_all("other");
    repo.write("other/config.ts", "export const SERVER = 'other';\n");
    repo.init_git();
    repo.commit_all("initial");
    for (value, import, proven) in [
        ("'new'", "import { SERVER } from './config';", true),
        ("12", "import { SERVER } from './config';", false),
        ("'new'", "import type { SERVER } from './config';", false),
        ("'new'", "import { type SERVER } from './config';", false),
        ("'new'", "import { SERVER } from './other/config';", false),
    ] {
        repo.write(
            "config.ts",
            config.replace("return 3", "return 4") + &format!("export const SERVER = {value};\n"),
        );
        repo.write("reader.ts", reader.replace("const SERVER = 'old';", import));
        let context = repo.run_json("context", &["--git"]);
        let high = callers(&context, "impacted_callers");
        assert!(high.contains(&("unchanged.ts".to_owned(), 1)), "{context}");
        assert_eq!(
            !high.contains(&("reader.ts".to_owned(), 1)),
            proven,
            "{context}"
        );
        if proven {
            assert!(
                callers(&context, "informational_callers").contains(&("reader.ts".to_owned(), 1)),
                "{context}"
            );
        }
    }
}

#[test]
fn impact_ts_rebound_const_literal_type_and_call_arguments_keep_blocking() {
    let repo = TestRepo::new();
    let config = "export function retry() {\n return 3;\n}\n";
    let before = r#"const SERVER = 'old';
export const link = `/${SERVER}`;
export const pinned: 'old' = SERVER;
function fixed(v: 'old') { return v; }
export const result = fixed(SERVER);
export const pinnedTemplate: '/old' = `/${SERVER}`;
"#;
    repo.write("config.ts", config);
    repo.write("user.ts", before);
    repo.init_git();
    repo.commit_all("initial");
    let changed = config.replace("return 3", "return 4") + "export const SERVER = 'old';\n";
    repo.write("config.ts", &changed);
    repo.write(
        "user.ts",
        before.replace(
            "const SERVER = 'old';",
            "import { SERVER } from './config';",
        ),
    );
    let hook = review_hook(&repo, false, 0);
    assert!(hook["impacts"].is_null(), "{hook}");
    repo.write("config.ts", changed.replace("'old'", "'new'"));
    for staged in [false, true] {
        if staged {
            repo.stage_all();
            repo.write("config.ts", config);
            repo.write("user.ts", before);
        }
        let hook = review_hook(&repo, staged, 1);
        let refs = hook["impacts"][0]["refs"].as_array().unwrap();
        assert_eq!(
            refs.iter()
                .map(|r| r["ln"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [2, 4, 5],
            "{hook}"
        );
    }
}

fn review_hook(repo: &TestRepo, staged: bool, code: i32) -> serde_json::Value {
    let mut command = cargo_bin();
    command
        .current_dir(repo.root())
        .args(["review", "--dir", ".", "--git", "--hook"]);
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(code), "{hook}");
    hook
}

fn callers(value: &serde_json::Value, bucket: &str) -> Vec<(String, u64)> {
    let mut result: Vec<_> = value["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| change[bucket].as_array().cloned().unwrap_or_default())
        .map(|caller| {
            (
                caller["path"].as_str().unwrap().to_owned(),
                caller["line"].as_u64().unwrap(),
            )
        })
        .collect();
    result.sort();
    result
}

#[test]
fn equivalent_named_imports_keep_only_actual_uses_blocking() {
    let repo = TestRepo::new();
    let before = "export const palette = { stable: 1, code: 2 };\nexport function render(text) { return text; }\n";
    repo.init_git();
    repo.write("layout.js", before);
    for (file, imported) in [
        ("plain.js", "palette"),
        ("same.ts", "palette as palette"),
        ("quoted.tsx", "\"palette\" as palette"),
        ("escaped.js", r#""\u0070alette" as palette"#),
    ] {
        repo.write(file, format!("import {{ {imported} }} from './layout';\nimport {{ \"render\" as render }} from './layout';\nexport const a = palette.stable;\nexport const b = palette.code;\nexport const c = render('a');\n"));
    }
    repo.write(
        "renamed.ts",
        "import { palette as other } from './layout';\nexport const b = other.code;\n",
    );
    repo.write(
        "escaped_read.ts",
        "import { palette as palette } from './layout';\nexport const b = \\u0070alette.code;\n",
    );
    repo.commit_all("base");
    repo.write(
        "layout.js",
        before
            .replace("code: 2", "code: 3")
            .replace("render(text)", "render(text, extra)"),
    );
    for staged in [false, true] {
        if staged {
            repo.stage_all();
            repo.write("layout.js", before);
        }
        let args: &[&str] = if staged {
            &["--git", "--staged"]
        } else {
            &["--git"]
        };
        let context = repo.run_json("context", args);
        let blocking = callers(&context, "impacted_callers");
        let info = callers(&context, "informational_callers");
        for file in ["plain.js", "same.ts", "quoted.tsx", "escaped.js"] {
            for line in 0..=2 {
                let site = (file.to_owned(), line);
                assert!(
                    info.contains(&site) && !blocking.contains(&site),
                    "{site:?}: {context}"
                );
            }
            for line in [3, 4] {
                assert!(
                    blocking.contains(&(file.to_owned(), line)),
                    "{file}:{line}: {context}"
                );
            }
        }
        for file in ["renamed.ts", "escaped_read.ts"] {
            assert!(
                blocking.contains(&(file.to_owned(), 0)),
                "{file}: {context}"
            );
        }
    }
}

#[test]
fn impact_rust_import_context_ignores_literal_and_comment_semicolons() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod internal;\npub mod user;\n");
    repo.write("src/internal.rs", "pub fn helper() -> u8 { 1 }\n");
    repo.write(
        "src/user.rs",
        r##"use crate::internal::helper;
fn show<T>(_: T, _: u8) {}
pub fn run() {
    show("; use fake", helper());
    show(r#"; use fake"#, helper());
    show(';', /*; use fake */ helper());
    show({ /*; use fake */ 1 }, helper());
    { use crate::internal::helper; show("; use fake", helper()); }
    println!("; use {:?}", helper());
}
"##,
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/internal.rs",
        "pub fn helper(value: u8) -> u8 { value }\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let impacted = callers(&context, "impacted_callers");
    for line in 3..=8 {
        assert!(
            impacted.contains(&("src/user.rs".to_owned(), line)),
            "{context}"
        );
    }
    let informational = callers(&context, "informational_callers");
    assert!(
        informational.contains(&("src/user.rs".to_owned(), 0)),
        "{context}"
    );
    for line in 3..=6 {
        assert!(
            !informational.contains(&("src/user.rs".to_owned(), line)),
            "{context}"
        );
    }
    let output = cargo_bin()
        .args(["impact", "--dir"])
        .arg(repo.root())
        .arg("--git")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
}

#[test]
fn impact_ts_import_context_ignores_literals_and_comments_but_keeps_imports() {
    let repo = TestRepo::new();
    repo.write("api.ts", "export function helper(): number { return 1; }\n");
    repo.write(
        "user.ts",
        r#"import { helper } from "./api";
export { helper };
const show = (...values: unknown[]) => values;
show("; import fake", helper());
show('; export { fake }', helper());
show(`; import fake`, helper());
show(/; import fake/, helper());
show(/*; import fake */ helper());
import { helper as other } from "./api"; show("; import fake", other());
"#,
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "api.ts",
        "export function helper(value: number): number { return value; }\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let impacted = callers(&context, "impacted_callers");
    for line in 3..=7 {
        assert!(
            impacted.contains(&("user.ts".to_owned(), line)),
            "{context}"
        );
    }
    let informational = callers(&context, "informational_callers");
    assert!(
        informational.contains(&("user.ts".to_owned(), 0)),
        "{context}"
    );
    assert!(
        informational.contains(&("user.ts".to_owned(), 1)),
        "{context}"
    );
    for line in 3..=7 {
        assert!(
            !informational.contains(&("user.ts".to_owned(), line)),
            "{context}"
        );
    }
}

#[test]
fn renamed_typescript_function_imports_report_only_calls_bound_to_the_changed_export() {
    let repo = TestRepo::new();
    repo.init_git();
    let before = "export function format(text: string): string { return text; }\n";
    repo.write("layout.ts", before);
    repo.write("independent.ts", before);
    repo.write(
        "barrel.ts",
        "export { format as pretty } from './layout.js';\n",
    );
    repo.write("reexport.ts", "import { pretty as other } from './barrel';\nexport const cb: (x: string) => string = other;\n");
    for (path, binding) in [
        ("direct.ts", "format as other"),
        ("quoted.tsx", r#""\u0066ormat" as other"#),
        ("other.mts", "format as o\\u0074her"),
    ] {
        repo.write(path, format!("import {{ {binding} }} from './layout.js';\nexport function run() {{ return (other)('a'); }}\nconst nested = (other: () => number) => other();\nexport const again = other?.('b');\n"));
    }
    repo.write(
        "independent-use.ts",
        "import { format as other } from './independent';\nexport const value = other('a');\n",
    );
    repo.write(
        "unrelated.ts",
        "const other = (text: string) => text;\nexport const value = other('a');\n",
    );
    repo.write("intrinsic.tsx", "import { format as other } from './layout';\ndeclare global { namespace JSX { interface IntrinsicElements { other: unknown } } }\nexport const view = <other/>;\n");
    repo.commit_all("before");
    repo.write(
        "layout.ts",
        before.replace("(text: string)", "(text: string, extra: string)"),
    );
    for staged in [false, true] {
        if staged {
            repo.stage_all();
            repo.write("layout.ts", before);
            repo.write("direct.ts", "// working tree only\n");
        }
        let args: &[&str] = if staged {
            &["--git", "--staged"]
        } else {
            &["--git"]
        };
        let context = repo.run_json("context", args);
        let sites = callers(&context, "impacted_callers");
        assert!(
            !sites.contains(&("intrinsic.tsx".to_owned(), 2)),
            "{context}"
        );
        assert!(sites.contains(&("reexport.ts".to_owned(), 1)), "{context}");
        for file in ["direct.ts", "quoted.tsx", "other.mts"] {
            for line in [1, 3] {
                assert!(
                    sites.contains(&(file.to_owned(), line)),
                    "{file}:{line} {context}"
                );
            }
            assert!(!sites.contains(&(file.to_owned(), 2)), "{context}");
        }
        assert!(
            !sites.iter().any(|(path, line)| path == "unrelated.ts"
                || (path == "independent-use.ts" && *line == 1)),
            "{context}"
        );
        let output = cargo_bin()
            .current_dir(repo.root())
            .args(["impact", "--dir", "."])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
    }
}
