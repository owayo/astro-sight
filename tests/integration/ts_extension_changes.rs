//! TS/TSX の拡張子変更と署名変更を、同じモジュールの変更として検査する。

use super::support::*;

fn fixture(old: &str, spec: &str, padding: bool) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    let pad = if padding {
        "// unchanged padding\n".repeat(30)
    } else {
        String::new()
    };
    repo.write(old, format!("{pad}export function formatGreeting(name: string): string {{\n  return `Hello, ${{name}}`;\n}}\n"));
    repo.write("src/app.ts", format!("import {{ formatGreeting }} from '{spec}';\n// astro-sight:allow-dead\nexport function main(): string {{\n  return formatGreeting('world');\n}}\n"));
    repo.init_git();
    repo.commit_all("base");
    repo
}

fn change(repo: &TestRepo, old: &str, new: &str, required: bool, padding: bool) {
    repo.remove_file(old);
    let pad = if padding {
        "// unchanged padding\n".repeat(30)
    } else {
        String::new()
    };
    let param = if required {
        "punctuation: string"
    } else {
        "punctuation = '!'"
    };
    repo.write(new, format!("{pad}export function formatGreeting(name: string, {param}): string {{\n  return `Hello, ${{name}}${{punctuation}}`;\n}}\n"));
}

fn has(entries: &serde_json::Value, name: &str) -> bool {
    entries.as_array().is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| entry["name"] == name || entry["n"] == name)
    })
}

#[test]
fn ts_extension_changes_keep_compatibility_and_real_impacts() {
    for (old, new) in [
        ("src/greet.ts", "src/greet.tsx"),
        ("src/greet.tsx", "src/greet.ts"),
    ] {
        for staged in [false, true] {
            for required in [false, true] {
                let repo = fixture(old, "./greet", false);
                change(&repo, old, new, required, false);
                if staged {
                    repo.stage_all();
                }
                let args = if staged {
                    vec!["--git", "--staged"]
                } else {
                    vec!["--git"]
                };
                let review = repo.run_json("review", &args);
                let api = &review["api_changes"];
                assert!(
                    !has(&api["removed"], "formatGreeting"),
                    "old={old}, staged={staged}: {review}"
                );
                assert!(
                    has(
                        &api[if required {
                            "modified"
                        } else {
                            "compatible_modified"
                        }],
                        "formatGreeting"
                    ),
                    "old={old}, staged={staged}, required={required}: {review}"
                );
                let mut hook_args = args.clone();
                hook_args.push("--hook");
                let output = cargo_bin()
                    .arg("review")
                    .arg("--dir")
                    .arg(repo.root())
                    .args(&hook_args)
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(i32::from(required)),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if required {
                    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                    assert!(
                        hook["blocking_categories"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|value| value == "impacts"),
                        "{hook}"
                    );
                }
            }
        }
    }
}

#[test]
fn ts_extension_changes_detected_renames_use_the_same_contract() {
    for (old, new) in [
        ("src/greet.ts", "src/greet.tsx"),
        ("src/greet.tsx", "src/greet.ts"),
    ] {
        for required in [false, true] {
            let repo = fixture(old, "./greet", true);
            change(&repo, old, new, required, true);
            repo.stage_all();
            let names = std::process::Command::new("git")
                .args(["diff", "--name-status", "--find-renames", "HEAD"])
                .current_dir(repo.root())
                .output()
                .unwrap();
            assert!(names.status.success());
            assert!(String::from_utf8_lossy(&names.stdout).starts_with('R'));
            for staged in [false, true] {
                let mut command = cargo_bin();
                command.args(["review", "--git", "--hook", "--dir"]);
                command.arg(repo.root());
                if staged {
                    command.arg("--staged");
                }
                let output = command.output().unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(i32::from(required)),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert!(!has(&hook["api"]["rm"], "formatGreeting"), "{hook}");
                assert!(
                    has(
                        &hook["api"][if required { "mod" } else { "mod_compat" }],
                        "formatGreeting"
                    ),
                    "{hook}"
                );
                if required {
                    assert!(
                        hook["blocking_categories"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|value| value == "impacts"),
                        "{hook}"
                    );
                }
            }
        }
    }
}

#[test]
fn ts_extension_changes_keep_explicit_old_specifiers_blocking() {
    for padding in [false, true] {
        let repo = fixture("src/greet.ts", "./greet.ts", padding);
        change(&repo, "src/greet.ts", "src/greet.tsx", false, padding);
        repo.stage_all();
        let output = cargo_bin()
            .args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(has(&hook["api"]["rm"], "formatGreeting"), "{hook}");
    }
}

#[test]
fn ts_extension_changes_audit_hidden_transports() {
    for transport in [
        "const value = require('./greet.ts');\n",
        "const value = r\\u0065quire('./greet.ts');\n",
        "const load = require; const value = load('./greet.ts');\n",
        "import value = require('./greet.ts');\n",
        "const value = import('./gre\\u0065t.ts');\n",
        "const path = './greet'; const value = import(path);\n",
        "export * from './greet.ts';\n",
    ] {
        let repo = fixture("src/greet.ts", "./greet", false);
        repo.write("src/loader.ts", transport);
        repo.commit_all("loader");
        change(&repo, "src/greet.ts", "src/greet.tsx", false, false);
        let output = cargo_bin()
            .args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{transport}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(
            has(&hook["api"]["rm"], "formatGreeting"),
            "{transport}: {hook}"
        );
    }
}

#[test]
fn ts_extension_changes_staged_sources_and_subdirectory_agree() {
    for staged in [false, true] {
        let repo = fixture("src/greet.ts", "./greet", false);
        change(&repo, "src/greet.ts", "src/greet.tsx", true, false);
        if staged {
            repo.stage_all();
            repo.write(
                "src/greet.tsx",
                "export function formatGreeting(name: string): string { return name; }\n",
            );
            repo.write("src/app.ts", "export const unrelated = 1;\n");
        }
        let args = if staged {
            vec!["--git", "--staged"]
        } else {
            vec!["--git"]
        };
        let review = repo.run_json_in("src", std::iter::empty::<(&str, &str)>(), "review", &args);
        assert!(
            !has(&review["api_changes"]["removed"], "formatGreeting"),
            "{review}"
        );
        assert!(
            has(&review["api_changes"]["modified"], "formatGreeting"),
            "{review}"
        );
        assert!(
            review["impact"]["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["path"] == "greet.tsx"
                    && change["impacted_callers"]
                        .as_array()
                        .is_some_and(|callers| callers
                            .iter()
                            .any(|caller| caller["path"] == "app.ts"))),
            "{review}"
        );
    }
}

#[test]
fn ts_extension_changes_hold_for_ignored_and_old_ancestor_configs() {
    for old_config in [false, true] {
        let repo = fixture("src/greet.ts", "./greet", false);
        let config = "{\"compilerOptions\":{\"moduleSuffixes\":[\".native\",\"\"]}}\n";
        if old_config {
            repo.write("tsconfig.json", config);
            repo.commit_all("resolver configuration");
            repo.remove_file("tsconfig.json");
        } else {
            repo.write(".gitignore", "tsconfig.json\n");
            repo.write("tsconfig.json", config);
        }
        change(&repo, "src/greet.ts", "src/greet.tsx", false, false);
        let review = repo.run_json_in(
            "src",
            std::iter::empty::<(&str, &str)>(),
            "review",
            &["--git"],
        );
        assert!(
            has(&review["api_changes"]["removed"], "formatGreeting"),
            "old_config={old_config}: {review}"
        );
    }
}

#[test]
fn ts_extension_changes_keep_unrelated_same_names_and_real_deletions() {
    for keep_ts in [false, true] {
        let repo = fixture("src/greet.ts", "./greet", false);
        if keep_ts {
            repo.write(
                "src/obsolete.ts",
                "export function formatGreeting(name: string): string { return name; }\n",
            );
            repo.commit_all("independent module");
            repo.remove_file("src/obsolete.ts");
        } else {
            repo.remove_file("src/greet.ts");
        }
        repo.write("src/other.tsx", "export function formatGreeting(name: string, punctuation = '!'): string { return name + punctuation; }\n");
        let review = repo.run_json("review", &["--git"]);
        assert!(
            !has(
                &review["api_changes"]["compatible_modified"],
                "formatGreeting"
            ),
            "{review}"
        );
        if !keep_ts {
            assert!(
                has(&review["api_changes"]["removed"], "formatGreeting"),
                "{review}"
            );
        }
    }
}

#[test]
fn ts_extension_changes_preserve_unicode_crlf_and_existing_ts_priority() {
    let repo = fixture("src/挨拶.ts", "./挨拶", false);
    let source = std::fs::read_to_string(repo.path("src/挨拶.ts"))
        .unwrap()
        .replace('\n', "\r\n");
    repo.write("src/挨拶.ts", source);
    repo.commit_all("CRLF source");
    change(&repo, "src/挨拶.ts", "src/挨拶.tsx", false, false);
    let source = std::fs::read_to_string(repo.path("src/挨拶.tsx"))
        .unwrap()
        .replace('\n', "\r\n");
    repo.write("src/挨拶.tsx", source);
    let review = repo.run_json("review", &["--git"]);
    assert!(
        has(
            &review["api_changes"]["compatible_modified"],
            "formatGreeting"
        ),
        "{review}"
    );

    let repo = fixture("src/greet.ts", "./greet", false);
    repo.write("src/greet.tsx", "export function formatGreeting(name: string, punctuation = '!'): string { return name + punctuation; }\n");
    let review = repo.run_json("review", &["--git"]);
    assert!(
        !has(
            &review["api_changes"]["compatible_modified"],
            "formatGreeting"
        ),
        "{review}"
    );
    assert!(
        has(&review["api_changes"]["added"], "formatGreeting"),
        "{review}"
    );
}

#[cfg(unix)]
#[test]
fn ts_extension_changes_audit_both_ancestor_alias_paths() {
    let repo = fixture("src/greet.ts", "./greet", false);
    change(&repo, "src/greet.ts", "src/greet.tsx", false, false);
    let alias_parent = tempfile::tempdir().unwrap();
    let alias = alias_parent.path().join("workspace");
    std::os::unix::fs::symlink(repo.root(), &alias).unwrap();
    let scope = alias.join("src");
    let review = repo.run_json_in(
        &scope,
        std::iter::empty::<(&str, &str)>(),
        "review",
        &["--git"],
    );
    assert!(
        has(
            &review["api_changes"]["compatible_modified"],
            "formatGreeting"
        ),
        "{review}"
    );
    std::fs::write(
        alias_parent.path().join("tsconfig.json"),
        "{\"compilerOptions\":{\"moduleSuffixes\":[\".native\",\"\"]}}\n",
    )
    .unwrap();
    let review = repo.run_json_in(
        &scope,
        std::iter::empty::<(&str, &str)>(),
        "review",
        &["--git"],
    );
    assert!(
        has(&review["api_changes"]["removed"], "formatGreeting"),
        "{review}"
    );
}
