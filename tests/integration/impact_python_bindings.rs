//! Python の同名直下関数と属性への書込みの帰属。

use super::support::*;

fn callers(result: &serde_json::Value, path: &str) -> Vec<(String, u64)> {
    result["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == path)
        .unwrap()["impacted_callers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|caller| {
            (
                caller["path"].as_str().unwrap().to_owned(),
                caller["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn namespace_lookup_keeps_same_named_local_callers_in_worktree_and_index() {
    let repo = TestRepo::new();
    let producer = "def total(x):\n    return x\n\ndef inside():\n    total = globals()['total']\n    return total(10)\n";
    let runner = "from producer import total\nfrom builtins import getattr as read\n\ndef run():\n    total = globals()['total']\n    return total(20)\n\ndef alias(module, key):\n    total = read(module, key)\n    return total(30)\n\ndef plain():\n    return total(40)\n\ndef local():\n    total = lambda x: x\n    return total(50)\n";
    repo.write("producer.py", producer);
    repo.write("runner.py", runner);
    repo.init_git();
    repo.commit_all("base");
    repo.write(
        "producer.py",
        producer.replace("total(x)", "total(x, required)"),
    );
    for args in [&["--git"][..], &["--git", "--staged"][..]] {
        if args.contains(&"--staged") {
            repo.stage_all();
        }
        let graph = repo.run_json("context", args);
        let actual = callers(&graph, "producer.py");
        for (path, line) in [
            ("producer.py", 5),
            ("runner.py", 5),
            ("runner.py", 9),
            ("runner.py", 12),
        ] {
            assert!(
                actual.contains(&(path.to_owned(), line)),
                "{path}:{line} {graph}"
            );
        }
        assert!(!actual.contains(&("runner.py".to_owned(), 4)), "{graph}");
        assert!(!actual.contains(&("runner.py".to_owned(), 8)), "{graph}");
        assert!(!actual.contains(&("runner.py".to_owned(), 16)), "{graph}");
    }
    let refs = repo.run_json("refs", &["--name", "total"]);
    assert!(
        refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "runner.py" && r["ln"] == 16),
        "{refs}"
    );
}

#[test]
fn impact_python_direct_local_function_and_attribute_write_are_not_callers() {
    for staged in [false, true] {
        let repo = TestRepo::new();
        repo.write("pipeline.py", "def resample():\n    return 0\n");
        repo.write(
            "preprocess.py",
            "def resample():\n    return 1\n\ndef run():\n    return resample()\n",
        );
        repo.write(
            "losses.py",
            "class Loss:\n    def __init__(self, transform):\n        self.resample = transform\n",
        );
        repo.write("consumer.py", "from pipeline import resample\nresample()\n");
        repo.init_git();
        repo.commit_all("initial");
        let refs_before = repo.run_json("refs", &["--name", "resample"]);
        repo.write("pipeline.py", "def resample(required):\n    return 0\n");
        if staged {
            repo.stage_all();
        }
        let args = if staged {
            &["--git", "--staged"][..]
        } else {
            &["--git"][..]
        };
        let context = repo.run_json("context", args);
        let actual = callers(&context, "pipeline.py");
        if staged {
            // caller は作業ツリーから読むため、index の新しい証拠としては使わない。
            assert!(
                actual.iter().any(|(path, _)| path == "preprocess.py"),
                "{context}"
            );
            assert!(
                actual.iter().any(|(path, _)| path == "losses.py"),
                "{context}"
            );
        } else {
            assert_eq!(actual, [("consumer.py".to_owned(), 1)], "{context}");
        }
        let review = repo.run_json("review", args);
        assert_eq!(
            callers(&review["impact"], "pipeline.py"),
            [("consumer.py".to_owned(), 1)],
            "{review}"
        );
        let refs_after = repo.run_json("refs", &["--name", "resample"]);
        let occurrences = |result: &serde_json::Value| {
            result["refs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    (
                        row["path"].clone(),
                        row["ln"].clone(),
                        row["col"].clone(),
                        row["kind"].clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(occurrences(&refs_before), occurrences(&refs_after));
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("consumer.py"), "{stderr}");
        if !staged {
            assert!(
                !stderr.contains("preprocess.py") && !stderr.contains("losses.py"),
                "{stderr}"
            );
        }

        repo.write(
            "consumer.py",
            "from pipeline import resample\nresample(1)\n",
        );
        if staged {
            repo.stage_all();
        }
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), !staged, "{output:?}");
    }
}

#[test]
fn impact_python_local_function_proof_preserves_ambiguous_bindings() {
    for consumer in [
        "def resample():\n    return 1\nresample = other\nresample()\n",
        "def resample():\n    return 1\nfrom pipeline import resample\nresample()\n",
        "from pipeline import resample\ndef resample():\n    return 1\nresample()\n",
        "def resample():\n    return 1\ndef resample():\n    return 2\nresample()\n",
        "@decorate\ndef resample():\n    return 1\nresample()\n",
        "if condition:\n    def resample():\n        return 1\nresample()\n",
        "def resample():\n    return 1\nfrom pipeline import *\nresample()\n",
        "def resample():\n    return 1\ndef overwrite():\n    global resample\n    resample = other\nresample()\n",
        "def resample():\n    return 1\nglobals().update(values)\nresample()\n",
        "resample()\ndef resample():\n    return 1\n",
        "def resample():\n    return 1\nresample()\ndef broken(\n",
    ] {
        let repo = TestRepo::new();
        repo.write("pipeline.py", "def resample():\n    return 0\n");
        repo.write("consumer.py", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("pipeline.py", "def resample(required):\n    return 0\n");
        let context = repo.run_json("context", &["--git"]);
        assert!(
            callers(&context, "pipeline.py")
                .iter()
                .any(|(path, _)| path == "consumer.py"),
            "{consumer}: {context}"
        );
    }
}

#[test]
fn impact_python_attribute_write_proof_preserves_reads_and_non_root_changes() {
    for consumer in [
        "obj.resample(1)\n",
        "callback = obj.resample\n",
        "obj.resample += value\n",
        "from pipeline import resample\nobj.resample = resample\n",
        "obj.resample = value\ndef broken(\n",
    ] {
        let repo = TestRepo::new();
        repo.write("pipeline.py", "def resample():\n    return 0\n");
        repo.write("consumer.py", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("pipeline.py", "def resample(required):\n    return 0\n");
        let context = repo.run_json("context", &["--git"]);
        assert!(
            !callers(&context, "pipeline.py").is_empty(),
            "{consumer}: {context}"
        );
    }
    for before in [
        "class Service:\n    def resample(self):\n        return 0\n",
        "@decorate\ndef resample():\n    return 0\n",
        "def resample():\n    return 0\nresample = decorate(resample)\n",
    ] {
        let repo = TestRepo::new();
        repo.write("pipeline.py", before);
        repo.write(
            "consumer.py",
            "from pipeline import Service\nobj.resample = value\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "pipeline.py",
            before
                .replace("resample(self)", "resample(self, required)")
                .replace("resample()", "resample(required)"),
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            !callers(&context, "pipeline.py").is_empty(),
            "{before}: {context}"
        );
    }
}
