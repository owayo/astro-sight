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
fn impact_python_from_import_alias_reports_the_call_and_preserves_refs() {
    let repo = TestRepo::new();
    repo.write("producer.py", "def total(x):\n    return x\n");
    repo.write("other.py", "def total(x):\n    return x\n");
    let runner = "from producer import (\n    total as calculate,\n)\n\ndef run():\n    return (calculate)(1)\n\ndef shadow(calculate):\n    return calculate(1)\n";
    repo.write("runner.py", runner);
    repo.write(
        "elsewhere.py",
        "from other import total as calculate\ncalculate(1)\n",
    );
    repo.write("unicode.py", "from producer import total as calculate\ndef run(ｃalculate=lambda x:x):\n    return calculate(1)\n");
    repo.write(
        "local.py",
        "def run():\n    from producer import total as work\n    return work(2)\n",
    );
    repo.init_git();
    repo.commit_all("base");
    repo.write("producer.py", "def total(x, required):\n    return x\n");
    for args in [&["--git"][..], &["--git", "--staged"][..]] {
        if args.contains(&"--staged") {
            repo.stage_all();
        }
        let context = repo.run_json("context", args);
        let mut actual = callers(&context, "producer.py");
        actual.sort();
        assert_eq!(
            actual,
            [("local.py".into(), 2), ("runner.py".into(), 5)],
            "{context}"
        );
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["symbols"][0] == "total" && row["name"] == "run")
        );
        let imports = context["changes"][0]["informational_callers"]
            .as_array()
            .unwrap();
        assert!(
            imports
                .iter()
                .any(|row| row["path"] == "runner.py" && row["line"] == 1)
        );
        let out = cargo_bin()
            .args(["impact", "--dir"])
            .arg(repo.root())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
    }
    let refs = repo.run_json("refs", &["--name", "total"]);
    let rows = refs["refs"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|row| row["path"] == "runner.py" && row["ln"] == 1)
    );
    assert!(
        !rows
            .iter()
            .any(|row| row["path"] == "runner.py" && row["ln"] == 5)
    );

    repo.write(
        "runner.py",
        runner.replace("(calculate)(1)", "(calculate)(1, 2)"),
    );
    repo.write(
        "local.py",
        "def run():\n    from producer import total as work\n    return work(2, 3)\n",
    );
    let impact = cargo_bin()
        .args(["impact", "--dir"])
        .arg(repo.root())
        .arg("--git")
        .output()
        .unwrap();
    assert!(impact.status.success(), "{impact:?}");

    repo.write("runner.py", runner.replace("(calculate)(1)", "calculate"));
    let context = repo.run_json("context", &["--git"]);
    assert!(
        callers(&context, "producer.py").contains(&("runner.py".into(), 1)),
        "{context}"
    );
}

#[test]
fn impact_python_import_alias_uses_the_evaluation_scope_of_expressions() {
    let repo = TestRepo::new();
    repo.write("producer.py", "def total(x): return x\n");
    let lines = [
        "from producer import total as calculate",
        "def run(other=lambda x: x, handles=()):",
        "    [calculate(x) for x in [1]]",
        "    (lambda: calculate(1))()",
        "    def inner(x=calculate(1)): pass",
        "    [calculate for calculate in [calculate(1)]]",
        "    (lambda calculate: calculate(1))(other)",
        "    [calculate(1) for calculate in handles]",
        "    (lambda calculate=calculate(1): calculate)()",
        "    other.calculate = other",
        "    calculate(1)",
        "    if handles:",
        "        from producer import total as inside",
        "        return inside(1)",
        "    return other(1)",
        "def fallback(other, handles):",
        "    if handles:",
        "        from producer import total as inside",
        "    else:",
        "        inside = other",
        "        return inside(1)",
    ];
    repo.write("runner.py", lines.join("\n"));
    repo.init_git();
    repo.commit_all("base");
    repo.write("producer.py", "def total(x, required): return x\n");
    let context = repo.run_json("context", &["--git"]);
    let mut rows: Vec<_> = callers(&context, "producer.py")
        .into_iter()
        .map(|(_, line)| line)
        .collect();
    rows.sort();
    assert_eq!(rows, [2, 3, 4, 5, 8, 10, 13], "{context}");
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
