//! 内包表記の束縛を、外部 namespace の証拠と混同しない。

use super::support::*;

const BEFORE: &str = "class Trail:\n    def join(self, part):\n        return part\n\ndef collect():\n    trail = Trail()\n    return trail.join(\"local\")\n";
const AFTER: &str = "class Trail:\n    def join(self, part, origin):\n        return origin + part\n\ndef collect():\n    trail = Trail()\n    return trail.join(\"local\", \"ok\")\n";
const FORMS: [(&str, &str, bool); 7] = [
    ("list", "[os.path.join(\"x\") for os in values]", false),
    ("set", "{os.path.join(\"x\") for os in values}", false),
    ("dict", "{os: os.path.join(\"x\") for os in values}", false),
    ("generator", "(os.path.join(\"x\") for os in values)", false),
    (
        "async_list",
        "[os.path.join(\"x\") async for os in values]",
        true,
    ),
    (
        "async_generator",
        "(os.path.join(\"x\") async for os in values)",
        true,
    ),
    (
        "tuple",
        "[os.path.join(\"x\") for os, extra in values]",
        false,
    ),
];

fn comprehension_repo(expression: &str, asynchronous: bool, shadowed: bool) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("routes");
    repo.write("routes/__init__.py", "");
    repo.write("routes/trail.py", BEFORE);
    let expression = if shadowed {
        expression.to_owned()
    } else {
        expression.replace("for os", "for entry")
    };
    let prefix = if asynchronous { "async " } else { "" };
    repo.write(
        "routes/consumer.py",
        format!("import os.path\n\n{prefix}def relay(values):\n    return {expression}\n"),
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("routes/trail.py", AFTER);
    repo
}

fn assert_comprehension_graph(graph: &serde_json::Value, expected: bool, label: &str) {
    let change = graph["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["path"] == "routes/trail.py")
        .unwrap_or_else(|| panic!("{label}: {graph}"));
    assert!(
        change["signature_changes"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| change["name"] == "join")),
        "{label}: {graph}"
    );
    let caller = change["impacted_callers"]
        .as_array()
        .is_some_and(|callers| {
            callers.iter().any(|caller| {
                caller["path"] == "routes/consumer.py"
                    && caller["symbols"]
                        .as_array()
                        .is_some_and(|names| names.iter().any(|name| name == "join"))
            })
        });
    assert_eq!(caller, expected, "{label}: {graph}");
    assert!(graph.get("truncations").is_none(), "{label}: {graph}");
}

fn assert_comprehension_direct(repo: &TestRepo, staged: bool, expected: bool, label: &str) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let context = repo.run_json("context", args);
    assert_comprehension_graph(&context, expected, label);
    let output = cargo_bin()
        .env("__CARGO_TEST_ROOT", repo.root().canonicalize().unwrap())
        .args(["impact", "--dir"])
        .arg(repo.root())
        .args(args)
        .output()
        .expect("impact");
    assert_eq!(
        output.status.code(),
        Some(i32::from(expected)),
        "{label}: {output:?}"
    );
    assert!(output.stdout.is_empty(), "{label}: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr).replace('\\', "/");
    if expected {
        assert!(stderr.contains("routes/consumer.py"), "{label}: {stderr}");
    } else {
        assert!(stderr.is_empty(), "{label}: {stderr}");
    }
    let refs = repo.run_json("refs", &["--name", "join"]);
    assert!(
        refs["refs"].as_array().unwrap().iter().any(|reference| {
            reference["path"] == "routes/consumer.py" && reference["kind"] == "ref"
        }),
        "{label}: {refs}"
    );
    assert!(refs.get("truncations").is_none(), "{label}: {refs}");
}

fn assert_comprehension_review(repo: &TestRepo, staged: bool, expected: bool, label: &str) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let review = repo.run_json("review", args);
    assert_comprehension_graph(&review["impact"], expected, label);
    assert!(review.get("truncations").is_none(), "{label}: {review}");
}

#[test]
fn comprehension_bindings_keep_unknown_method_callers_in_worktree_and_index() {
    for (label, expression, asynchronous) in FORMS {
        let repo = comprehension_repo(expression, asynchronous, true);
        assert_comprehension_direct(&repo, false, true, label);
        assert_comprehension_review(&repo, false, true, label);
        repo.stage_all();
        repo.write("routes/trail.py", BEFORE);
        assert_comprehension_direct(&repo, true, true, label);
        assert_comprehension_review(&repo, true, true, label);
    }
}

#[test]
fn comprehension_bindings_preserve_proven_namespace_exclusions() {
    for (label, expression, asynchronous) in FORMS {
        let repo = comprehension_repo(expression, asynchronous, false);
        assert_comprehension_direct(&repo, false, false, label);
        assert_comprehension_review(&repo, false, false, label);
        repo.stage_all();
        repo.write("routes/trail.py", BEFORE);
        // staged の namespace の証明は、index の写しを読む review で確かめる。
        assert_comprehension_review(&repo, true, false, label);
    }
}
