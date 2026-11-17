//! C++ の受け手が型名を直接書かなくても、メソッドの候補を保持する。

use super::support::*;

const BEFORE: &str =
    "class Tank {\npublic:\n    int feed() { return 0; }\n};\ninline Tank make() { return {}; }\n";
const AFTER: &str = "class Tank {\npublic:\n    int feed(int amount) { return amount; }\n};\ninline Tank make() { return {}; }\n";
const INFERRED: [(&str, &str); 3] = [
    ("auto", "auto tank = make(); return tank.feed();"),
    ("factory", "return make().feed();"),
    (
        "alias",
        "using Local = decltype(make()); Local tank = make(); return tank.feed();",
    ),
];

fn factory_repo(body: &str, staged: bool) -> TestRepo {
    let repo = TestRepo::new();
    repo.write("tank.hpp", BEFORE);
    repo.write(
        "consumer.cpp",
        format!("#include \"tank.hpp\"\nint run() {{ {body} }}\n"),
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("tank.hpp", AFTER);
    if staged {
        repo.stage_all();
        // index だけが新しい契約を持つようにする。
        repo.write("tank.hpp", BEFORE);
    }
    repo
}

fn assert_factory_graph(graph: &serde_json::Value, label: &str) {
    let change = graph["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["path"] == "tank.hpp")
        .unwrap_or_else(|| panic!("{label}: {graph}"));
    assert!(
        change["signature_changes"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| {
                change["name"] == "feed"
                    && change["new_signature"]
                        .as_str()
                        .is_some_and(|signature| signature.contains("feed(int amount)"))
            })),
        "{label}: {graph}"
    );
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| {
                callers.iter().any(|caller| {
                    caller["path"] == "consumer.cpp"
                        && caller["symbols"]
                            .as_array()
                            .is_some_and(|names| names.iter().any(|name| name == "feed"))
                })
            }),
        "{label}: {graph}"
    );
    assert!(graph.get("truncations").is_none(), "{label}: {graph}");
}

fn assert_factory_callers(repo: &TestRepo, staged: bool, type_written: bool, label: &str) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let context = repo.run_json("context", args);
    assert_factory_graph(&context, label);
    let review = repo.run_json("review", args);
    assert_factory_graph(&review["impact"], label);
    assert!(review.get("truncations").is_none(), "{label}: {review}");
    let output = cargo_bin()
        .env("__CARGO_TEST_ROOT", repo.root().canonicalize().unwrap())
        .args(["impact", "--dir"])
        .arg(repo.root())
        .args(args)
        .output()
        .expect("impact");
    assert_eq!(output.status.code(), Some(1), "{label}: {output:?}");
    assert!(output.stdout.is_empty(), "{label}: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr).replace('\\', "/");
    assert!(stderr.contains("consumer.cpp"), "{label}: {stderr}");
    for (name, expected) in [("feed", true), ("Tank", type_written)] {
        let refs = repo.run_json("refs", &["--name", name]);
        assert_eq!(
            refs["refs"].as_array().unwrap().iter().any(|reference| {
                reference["path"] == "consumer.cpp" && reference["kind"] == "ref"
            }),
            expected,
            "{label}/{name}: {refs}"
        );
        assert!(refs.get("truncations").is_none(), "{label}: {refs}");
        assert!(refs.get("result_summary").is_none(), "{label}: {refs}");
    }
}

#[test]
fn cpp_inferred_receivers_keep_worktree_method_callers() {
    for (label, body) in INFERRED {
        let repo = factory_repo(body, false);
        assert_factory_callers(&repo, false, false, label);
    }
}

#[test]
fn cpp_inferred_receivers_keep_index_method_callers() {
    for (label, body) in INFERRED {
        let repo = factory_repo(body, true);
        assert_factory_callers(&repo, true, false, label);
    }
}

#[test]
fn cpp_inferred_receivers_preserve_explicit_type_control() {
    for staged in [false, true] {
        let repo = factory_repo("Tank tank = make(); return tank.feed();", staged);
        assert_factory_callers(&repo, staged, true, "explicit type");
    }
}
