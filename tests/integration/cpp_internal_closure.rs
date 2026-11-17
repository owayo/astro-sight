use super::support::*;

fn source(updated: bool, stale: bool) -> String {
    let parameters = if updated {
        "int amount, bool keep"
    } else {
        "int amount"
    };
    let body = if updated {
        "if (keep) total += amount;"
    } else {
        "total += amount;"
    };
    let arguments = if updated && !stale { "3, true" } else { "3" };
    format!(
        "#include <cstdio>\nnamespace {{\nstruct Tank {{\n    int total = 0;\n    bool feed({parameters}) {{\n        {body}\n        return total > 0;\n    }}\n}};\n}}\nint main() {{\n    Tank tank;\n    if (!tank.feed({arguments})) return 1;\n    printf(\"%d\\n\", tank.total);\n    return 0;\n}}\n"
    )
}

fn fixture(python: bool) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("gauge.cpp", source(false, false));
    if python {
        repo.write("pets.py", "class Bowl:\n    def feed(self, grams):\n        return grams > 0\n\ndef morning(bowl):\n    return bowl.feed(40)\n");
    }
    repo.commit_all("initial");
    repo
}

fn assert_review(repo: &TestRepo, staged: bool, blocking: bool) {
    let mut args = vec!["--git", "--format", "json"];
    if staged {
        args.push("--staged");
    }
    let normal = repo.run_json("review", &args);
    let modified = normal["api_changes"]["modified"].as_array().unwrap();
    assert_eq!(
        modified.iter().any(|change| change["name"] == "Tank.feed"),
        blocking,
        "staged={staged}: {normal}"
    );
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(i32::from(blocking)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if blocking {
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(
            hook["api"]["mod"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["n"] == "Tank.feed"),
            "{hook}"
        );
        assert!(
            hook["blocking_categories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|category| category == "api.mod"),
            "{hook}"
        );
    }
}

#[test]
fn cpp_internal_closure_updated_calls_ignore_unrelated_python_names() {
    for python in [false, true] {
        let repo = fixture(python);
        repo.write("gauge.cpp", source(true, false));
        assert_review(&repo, false, false);
        let refs = repo.run_json(
            "refs",
            &[
                "--name",
                "feed",
                "--max-results",
                "unlimited",
                "--token-budget",
                "unlimited",
            ],
        );
        let refs = refs["refs"].as_array().unwrap();
        assert_eq!(
            refs.iter()
                .filter(|reference| reference["path"] == "gauge.cpp")
                .count(),
            2
        );
        assert_eq!(
            refs.iter()
                .filter(|reference| reference["path"] == "pets.py")
                .count(),
            if python { 2 } else { 0 }
        );
    }
}

#[test]
fn cpp_internal_closure_stale_calls_block_with_or_without_python() {
    for python in [false, true] {
        let repo = fixture(python);
        repo.write("gauge.cpp", source(true, true));
        assert_review(&repo, false, true);
    }
}

#[test]
fn cpp_internal_closure_staged_calls_use_index_in_both_directions() {
    for stale in [false, true] {
        let repo = fixture(true);
        repo.write("gauge.cpp", source(true, stale));
        repo.stage_all();
        repo.write("gauge.cpp", source(true, !stale));
        assert_review(&repo, true, stale);
        assert_review(&repo, false, !stale);
    }
}

#[test]
fn cpp_internal_closure_minimal_return_call_requires_contract_proof() {
    for stale in [false, true] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("gauge.cpp", "namespace {\nstruct Tank {\nbool feed(int amount) { return amount > 0; }\n};\n}\nint main() {\nTank tank;\nreturn tank.feed(3) ? 0 : 1;\n}\n");
        repo.commit_all("initial");
        let arguments = if stale { "3" } else { "3, true" };
        repo.write("gauge.cpp", format!("namespace {{\nstruct Tank {{\nbool feed(int amount, bool keep) {{ return keep && amount > 0; }}\n}};\n}}\nint main() {{\nTank tank;\nreturn tank.feed({arguments}) ? 0 : 1;\n}}\n"));
        assert_review(&repo, false, stale);
    }
}

#[test]
fn cpp_internal_closure_unproven_uses_stay_blocking_when_callers_change() {
    for (needle, replacement) in [
        ("return 0;", "store(tank); return 0;"),
        ("return 0;", "register_type<Tank>(); return 0;"),
        ("return 0;", "auto pointer = &Tank::feed; return 0;"),
        ("printf(\"%d\\n\", tank.total)", "store(tank.total)"),
        ("Tank tank;", "int printf; Tank tank;"),
        ("namespace {", "namespace exposed {"),
    ] {
        let repo = fixture(false);
        repo.write(
            "gauge.cpp",
            source(true, false).replace(needle, replacement),
        );
        assert_review(&repo, false, true);
    }
}

#[test]
fn cpp_internal_closure_old_escape_and_staged_escape_remain_unproven() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "gauge.cpp",
        source(false, false).replace("return 0;", "register_type<Tank>(); return 0;"),
    );
    repo.commit_all("initial");
    repo.write("gauge.cpp", source(true, false));
    assert_review(&repo, false, true);
    for escape in [false, true] {
        let repo = fixture(false);
        let plain = source(true, false);
        let escaped = plain.replace("return 0;", "register_type<Tank>(); return 0;");
        repo.write("gauge.cpp", if escape { &escaped } else { &plain });
        repo.stage_all();
        repo.write("gauge.cpp", if escape { &plain } else { &escaped });
        assert_review(&repo, true, escape);
        assert_review(&repo, false, !escape);
    }
}

#[test]
fn cpp_internal_closure_cross_file_cpp_and_unknown_uses_remain_blocking() {
    for (path, consumer) in [
        (
            "other.cpp",
            "#include \"gauge.cpp\"\nbool use() { Tank tank; return tank.feed(3); }\n",
        ),
        (
            "other.cpp",
            "#include \"gauge.cpp\"\nvoid expose() { register_type<Tank>(); }\n",
        ),
        ("other.c", "void use() { feed(3); }\n"),
        (
            "other.cpp",
            "#include \"gauge.cpp\"\nvoid expose() { bind(&Tank::feed); }\n",
        ),
        ("other.vue", "<script>registerType(Tank)</script>\n"),
        (
            "other.py",
            "import native\nvalue = native.Tank()\nvalue.feed(3)\n",
        ),
        (
            "other.cpp",
            "#include \"gauge.cpp\"\nvoid use( { tank.feed(3);\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("gauge.cpp", source(false, false));
        repo.write(path, consumer);
        repo.commit_all("initial");
        repo.write("gauge.cpp", source(true, false));
        assert_review(&repo, false, true);
    }
}

#[test]
fn cpp_internal_closure_changed_old_cpp_consumers_stay_unproven() {
    for action in ["replace", "delete", "rename"] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("gauge.cpp", source(false, false));
        repo.write(
            "other.cpp",
            "#include \"gauge.cpp\"\nvoid expose() { register_type<Tank>(); }\n",
        );
        repo.commit_all("initial");
        match action {
            "replace" => repo.write("other.cpp", "int unrelated() { return 0; }\n"),
            "delete" => repo.remove_file("other.cpp"),
            "rename" => repo.git(["mv", "other.cpp", "consumer.py"]),
            _ => unreachable!(),
        }
        repo.write("gauge.cpp", source(true, false));
        assert_review(&repo, false, true);
    }
}

#[test]
fn cpp_internal_closure_ignored_and_generated_tracked_cpp_stays_unproven() {
    for setting in [
        "existing_ignore",
        "new_ignore",
        "existing_generated",
        "new_generated",
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("gauge.cpp", source(false, false));
        repo.write(
            "wrapper.cpp",
            "#include \"gauge.cpp\"\nbool call() { Tank tank; return tank.feed(3); }\n",
        );
        if setting == "existing_ignore" {
            repo.write(".gitignore", "wrapper.cpp\n");
        }
        if setting == "existing_generated" {
            repo.write(".gitattributes", "wrapper.cpp linguist-generated\n");
        }
        repo.git(["add", "-f", "wrapper.cpp"]);
        repo.commit_all("initial");
        if setting == "new_ignore" {
            repo.write(".gitignore", "wrapper.cpp\n");
        }
        if setting == "new_generated" {
            repo.write(".gitattributes", "wrapper.cpp linguist-generated\n");
        }
        repo.write("gauge.cpp", source(true, false));
        assert_review(&repo, false, true);
    }
}

#[test]
fn cpp_internal_closure_scope_budget_boundaries_remain_conservative() {
    let repo = fixture(false);
    for index in 0..255 {
        repo.write(format!("padding{index}.txt"), "");
    }
    repo.commit_all("padding");
    repo.write("gauge.cpp", source(true, false));
    assert_review(&repo, false, false);
    repo.write("extra.txt", "");
    assert_review(&repo, false, true);

    let repo = fixture(false);
    let padding = " ".repeat(1024 * 1024);
    for index in 0..8 {
        repo.write(format!("padding{index}.txt"), &padding);
    }
    repo.commit_all("padding");
    repo.write("gauge.cpp", source(true, false));
    assert_review(&repo, false, false);
    repo.write("extra.txt", " ");
    assert_review(&repo, false, true);
    repo.remove_file("extra.txt");
    repo.write("padding0.txt", format!("{padding} "));
    assert_review(&repo, false, true);
}

#[test]
fn cpp_internal_closure_multiple_methods_keep_all_changes_blocking() {
    let repo = TestRepo::new();
    repo.init_git();
    let source = |updated: bool| {
        let mut methods = String::new();
        let mut calls = String::new();
        for index in 0..16 {
            let parameters = if updated {
                "int amount, bool keep"
            } else {
                "int amount"
            };
            let body = if updated {
                "keep && amount > 0"
            } else {
                "amount > 0"
            };
            let arguments = if updated { "3, true" } else { "3" };
            methods.push_str(&format!(
                "bool feed{index}({parameters}) {{ return {body}; }}\n"
            ));
            calls.push_str(&format!("if (!tank.feed{index}({arguments})) return 1;\n"));
        }
        format!(
            "namespace {{\nstruct Tank {{\n{methods}}};\n}}\nint main() {{ Tank tank;\n{calls}return 0; }}\n"
        )
    };
    repo.write("gauge.cpp", source(false));
    repo.commit_all("initial");
    repo.write("gauge.cpp", source(true));
    let normal = repo.run_json("review", &["--git", "--format", "json"]);
    let modified = normal["api_changes"]["modified"].as_array().unwrap();
    assert_eq!(modified.len(), 16, "{normal}");
    for index in 0..16 {
        let name = format!("Tank.feed{index}");
        assert!(
            modified.iter().any(|change| change["name"] == name),
            "{normal}"
        );
    }
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
    assert_eq!(hook["api"]["mod"].as_array().unwrap().len(), 16, "{hook}");
}

#[test]
fn cpp_internal_closure_subdirectory_scope_cannot_prove_closed_uses() {
    for external_cpp in [false, true] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("tool");
        repo.write("tool/gauge.cpp", source(false, false));
        repo.write(
            "tool/pets.py",
            "class Bowl:\n    def feed(self, grams):\n        return grams > 0\n",
        );
        if external_cpp {
            repo.write(
                "wrapper.cpp",
                "#include \"tool/gauge.cpp\"\nbool use() { Tank other; return other.feed(3); }\n",
            );
        }
        repo.commit_all("initial");
        repo.write("tool/gauge.cpp", source(true, false));
        let directory = repo.path("tool");
        let output = cargo_bin()
            .args(["review", "--git", "--format", "json", "--dir"])
            .arg(&directory)
            .output()
            .unwrap();
        assert!(output.status.success());
        let normal: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            normal["api_changes"]["modified"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["name"] == "Tank.feed"),
            "external_cpp={external_cpp}: {normal}"
        );
        let output = cargo_bin()
            .args(["review", "--git", "--hook", "--dir"])
            .arg(&directory)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "external_cpp={external_cpp}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn cpp_internal_closure_unsupported_sources_do_not_need_literal_names_to_block() {
    for action in ["retain", "delete", "rename"] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("gauge.cpp", source(false, false));
        let wrapper = "#include \"gauge.cpp\"\n#define JOIN(a,b) a##b\nbool use() { JOIN(Ta,nk) other; return other.JOIN(fe,ed)(3); }\n";
        assert!(!wrapper.contains("Tank") && !wrapper.contains("feed"));
        repo.write("wrapper.mm", wrapper);
        repo.commit_all("initial");
        match action {
            "retain" => {}
            "delete" => repo.remove_file("wrapper.mm"),
            "rename" => repo.git(["mv", "wrapper.mm", "wrapper.txt"]),
            _ => unreachable!(),
        }
        repo.write("gauge.cpp", source(true, false));
        assert_review(&repo, false, true);
    }
}
