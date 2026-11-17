//! index の写しが省いた import 元を、外部 namespace の証拠にしない。

use super::support::*;

const BEFORE: &str = "class Trail:\n    def join(self, part):\n        return part\n\ndef collect():\n    trail = Trail()\n    return trail.join(\"local\")\n";
const AFTER: &str = "class Trail:\n    def join(self, part, origin):\n        return origin + part\n\ndef collect():\n    trail = Trail()\n    return trail.join(\"local\", \"ok\")\n";

fn snapshot_repo(consumer: &str, entry: Option<(&str, &str)>) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("routes");
    repo.write("routes/__init__.py", "");
    repo.write("routes/trail.py", BEFORE);
    repo.write("routes/consumer.py", consumer);
    repo.init_git();
    repo.commit_all("initial");
    if let Some((path, mode)) = entry {
        // Git の mode だけを置き、OS の symlink 作成権限に依存させない。
        let output = if mode == "160000" {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo.root())
                .output()
                .unwrap()
        } else {
            repo.write("link-target.txt", "../outside.py");
            std::process::Command::new("git")
                .args(["hash-object", "-w", "link-target.txt"])
                .current_dir(repo.root())
                .output()
                .unwrap()
        };
        assert!(output.status.success(), "{output:?}");
        let oid = String::from_utf8(output.stdout).unwrap();
        repo.git([
            "update-index",
            "--add",
            "--cacheinfo",
            mode,
            oid.trim(),
            path,
        ]);
        repo.git(["commit", "-m", "opaque namespace", "-q"]);
    }
    repo.write("routes/trail.py", AFTER);
    repo.git(["add", "routes/trail.py"]);
    // 新契約は index に置き、worktree は旧契約へ戻す。
    repo.write("routes/trail.py", BEFORE);
    repo
}

fn assert_snapshot_graph(graph: &serde_json::Value, expected: bool, label: &str) {
    let change = graph["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["path"] == "routes/trail.py")
        .unwrap_or_else(|| panic!("{label}: {graph}"));
    assert!(
        change["signature_changes"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| {
                change["name"] == "join"
                    && change["new_signature"] == "def join(self, part, origin):"
            })),
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

fn assert_snapshot_review(repo: &TestRepo, expected: bool, label: &str) {
    let review = repo.run_json("review", &["--git", "--staged"]);
    assert_snapshot_graph(&review["impact"], expected, label);
    assert!(review.get("truncations").is_none(), "{label}: {review}");
    let refs = repo.run_json("refs", &["--name", "join"]);
    assert!(
        refs["refs"].as_array().unwrap().iter().any(|reference| {
            reference["path"] == "routes/consumer.py" && reference["kind"] == "ref"
        }),
        "{label}: {refs}"
    );
    assert!(refs.get("truncations").is_none(), "{label}: {refs}");
    assert!(refs.get("result_summary").is_none(), "{label}: {refs}");
}

fn assert_snapshot_direct(repo: &TestRepo, label: &str) {
    let context = repo.run_json("context", &["--git", "--staged"]);
    assert_snapshot_graph(&context, true, label);
    let output = cargo_bin()
        .env("__CARGO_TEST_ROOT", repo.root().canonicalize().unwrap())
        .args(["impact", "--dir"])
        .arg(repo.root())
        .args(["--git", "--staged"])
        .output()
        .expect("impact");
    assert_eq!(output.status.code(), Some(1), "{label}: {output:?}");
    assert!(output.stdout.is_empty(), "{label}: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr).replace('\\', "/");
    assert!(stderr.contains("routes/consumer.py"), "{label}: {stderr}");
}

#[test]
fn staged_opaque_namespaces_keep_unknown_method_callers() {
    for (path, mode) in [
        ("helper.py", "120000"),
        ("helper/__init__.py", "120000"),
        ("custom.pth", "120000"),
        ("helper", "160000"),
    ] {
        let repo = snapshot_repo(
            "import helper\n\ndef relay(part):\n    return helper.join(part)\n",
            Some((path, mode)),
        );
        assert_snapshot_review(&repo, true, path);
        assert_snapshot_direct(&repo, path);
    }
}

#[test]
fn staged_regular_namespaces_preserve_proven_exclusions() {
    let repo = snapshot_repo(
        "import os.path\n\ndef relay(part):\n    return os.path.join(part)\n",
        None,
    );
    assert_snapshot_review(&repo, false, "regular namespace");
}

#[test]
fn staged_opaque_namespaces_preserve_literal_proofs() {
    let repo = snapshot_repo(
        "def relay(parts):\n    return \",\".join(parts)\n",
        Some(("helper.py", "120000")),
    );
    assert_snapshot_review(&repo, false, "literal with opaque namespace");
}
