//! 混在 hunk の新規 Python クラスを既存宣言の変更へ混ぜない。

use super::support::*;

const PROTOCOLS: &str = "from typing import Protocol\n\n\nclass Reader(Protocol):\n    def read(self, size: int) -> bytes: ...\n\n\nclass Writer(Protocol):\n    def write(self, data: bytes) -> int: ...\n\n\nclass Closer(Protocol):\n    def close(self) -> None: ...\n\n\nclass Flusher(Protocol):\n    def flush(self) -> None: ...\n";
const COUNTER: &str = "\n\nclass Counter(Protocol):\n    def count(self, text: str) -> int: ...\n";
const TRAIL: &str = "class _Trail:\n    prefix = \"/\"\n\n    def join(self, part):\n        return self.prefix + part\n\n\ndef collect(parts):\n    trail = _Trail()\n    return [trail.join(part) for part in parts]\n";
const TRAIL_CHANGED: &str = "class _Trail:\n    prefix = \"/\"\n\n    def join(self, part, origin):\n        return origin + self.prefix + part\n\n\nclass _Bundle:\n    level = 0\n\n    def join(self, part, origin):\n        return origin + str(self.level) + part\n\n\ndef collect(parts):\n    trail = _Trail()\n    bundle = _Bundle()\n    return [trail.join(part, \"local\") + bundle.join(part, \"local\") for part in parts]\n";
const LABELS: &str = "import os.path\n\n\ndef label(words: list[str]) -> str:\n    return \", \".join(words)\n\n\ndef location(root: str, name: str) -> str:\n    return os.path.join(root, name)\n\n\ndef chain(words):\n    out = []\n\n    def join(word):\n        out.append(word.strip())\n\n    for word in words:\n        join(word)\n    return out\n";

fn protocols_repo(prefix: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("app");
    repo.write("app/__init__.py", "");
    repo.write("app/protocols.py", format!("{prefix}{PROTOCOLS}"));
    repo.write("app/words.py", "from collections import Counter\n\n\ndef tally(words: list[str]) -> Counter[str]:\n    return Counter(words)\n");
    repo.write("app/tallies.py", "import collections\n\n\ndef add_tallies(first: collections.Counter, second: collections.Counter) -> collections.Counter:\n    return first + second\n");
    repo.init_git();
    repo.commit_all("initial");
    repo
}

fn trail_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("routes");
    repo.write("routes/__init__.py", "");
    repo.write("routes/trail.py", TRAIL);
    repo.write("routes/labels.py", LABELS);
    repo.init_git();
    repo.commit_all("initial");
    repo
}

fn changed_file(context: &serde_json::Value, path: &str) -> serde_json::Value {
    context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == path)
        .unwrap()
        .clone()
}

fn assert_change(file: &serde_json::Value, name: &str, kind: &str, changes: &[&str]) {
    let actual: Vec<_> = file["affected_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|symbol| symbol["name"] == name && symbol["kind"] == kind)
        .map(|symbol| symbol["change_type"].as_str().unwrap())
        .collect();
    assert_eq!(actual, changes, "{file}");
}

fn assert_no_impacts(repo: &TestRepo, args: &[&str]) -> serde_json::Value {
    let staged = args.contains(&"--staged");
    let context = if staged {
        repo.run_json("review", args)["impact"].clone()
    } else {
        repo.run_json("context", args)
    };
    for change in context["changes"].as_array().unwrap() {
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{context}"
        );
    }
    if !staged {
        let output = cargo_bin()
            .args(["impact", "--dir"])
            .arg(repo.root())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
    }
    context
}

#[test]
fn python_added_class_in_mixed_hunk_is_not_an_existing_counter() {
    for staged in [false, true] {
        let repo = protocols_repo("");
        let before = repo.run_json("refs", &["--name", "Counter"]);
        repo.write(
            "app/protocols.py",
            format!(
                "{}{COUNTER}",
                PROTOCOLS.replace("flush(self)", "flush(self, force: bool = False)")
            ),
        );
        if staged {
            repo.stage_all();
        }
        let args = if staged {
            &["--git", "--staged"][..]
        } else {
            &["--git"][..]
        };
        let context = assert_no_impacts(&repo, args);
        let file = changed_file(&context, "app/protocols.py");
        assert_change(&file, "Counter", "class", &["added"]);
        assert_change(&file, "count", "function", &["added"]);
        let review = repo.run_json("review", args);
        assert!(
            review["api_changes"]["added"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == "Counter"),
            "{review}"
        );
        let output = cargo_bin()
            .args(["review", "--dir"])
            .arg(repo.root())
            .args(args)
            .arg("--hook")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(hook["impacts"].is_null(), "{hook}");
        let after = repo.run_json("refs", &["--name", "Counter"]);
        let external = |json: &serde_json::Value| {
            json["refs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["path"] != "app/protocols.py")
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(external(&before), external(&after));
    }
}

#[test]
fn python_added_same_name_method_preserves_changed_method_owner() {
    for staged in [false, true] {
        let repo = trail_repo();
        let before = repo.run_json("refs", &["--name", "join"]);
        repo.write("routes/trail.py", TRAIL_CHANGED);
        if staged {
            repo.stage_all();
        }
        let args = if staged {
            &["--git", "--staged"][..]
        } else {
            &["--git"][..]
        };
        let context = assert_no_impacts(&repo, args);
        let file = changed_file(&context, "routes/trail.py");
        assert_change(&file, "join", "function", &["modified", "added"]);
        assert!(
            file["signature_changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == "join"),
            "{file}"
        );
        let review = repo.run_json("review", args);
        assert!(
            changed_file(&review["impact"], "routes/trail.py")["impacted_callers"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{review}"
        );
        let after = repo.run_json("refs", &["--name", "join"]);
        let external = |json: &serde_json::Value| {
            json["refs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["path"] == "routes/labels.py")
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(external(&before), external(&after));
    }
}

#[test]
fn python_added_class_keeps_split_and_add_only_controls_compatible() {
    for changed in [
        format!("{PROTOCOLS}{COUNTER}"),
        format!(
            "{}{COUNTER}",
            PROTOCOLS.replace(
                "read(self, size: int)",
                "read(self, size: int, offset: int = 0)"
            )
        ),
    ] {
        let repo = protocols_repo("");
        repo.write("app/protocols.py", changed);
        assert_no_impacts(&repo, &["--git"]);
    }
    let new = TRAIL_CHANGED
        .replacen("def join(self, part, origin):", "def join(self, part):", 1)
        .replacen(
            "return origin + self.prefix + part",
            "return part + self.prefix",
            1,
        )
        .replace("trail.join(part, \"local\")", "trail.join(part)");
    let renamed = TRAIL_CHANGED
        .replace(
            "class _Bundle:\n    level = 0\n\n    def join",
            "class _Bundle:\n    level = 0\n\n    def strip",
        )
        .replace("bundle.join", "bundle.strip");
    for changed in [new, renamed] {
        let repo = trail_repo();
        repo.write("routes/trail.py", changed);
        assert_no_impacts(&repo, &["--git"]);
    }
}

#[test]
fn python_added_same_name_method_keeps_actual_existing_callers_blocking() {
    for bundle_first in [false, true] {
        let repo = trail_repo();
        repo.write("routes/consumer.py", "from routes.trail import _Trail\n\ndef use():\n    trail = _Trail()\n    return trail.join(\"part\")\n");
        repo.commit_all("consumer");
        let mut changed = TRAIL_CHANGED.to_owned();
        if bundle_first {
            let bundle = "class _Bundle:\n    level = 0\n\n    def join(self, part, origin):\n        return origin + str(self.level) + part\n\n\n";
            changed = format!("{bundle}{}", changed.replace(bundle, ""));
        }
        repo.write("routes/trail.py", changed);
        let context = repo.run_json("context", &["--git"]);
        let file = changed_file(&context, "routes/trail.py");
        assert!(
            file["signature_changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == "join"),
            "{file}"
        );
        assert!(
            file["impacted_callers"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["path"] == "routes/consumer.py")),
            "{file}"
        );
        assert!(
            file["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["path"] != "routes/labels.py"),
            "{file}"
        );
        let output = cargo_bin()
            .args(["impact", "--dir"])
            .arg(repo.root())
            .arg("--git")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
    }
}

#[test]
fn python_added_class_does_not_hide_replaced_import_or_assignment_bindings() {
    for old_binding in [
        "from collections import Counter\n",
        "Counter = other\n",
        "from library import *\n",
        "exec(code)\n",
    ] {
        let repo = protocols_repo(old_binding);
        repo.write(
            "app/consumer.py",
            "from app.protocols import Counter\nvalue = Counter()\n",
        );
        repo.commit_all("existing binding");
        repo.write(
            "app/protocols.py",
            format!(
                "{}{COUNTER}",
                PROTOCOLS.replace("flush(self)", "flush(self, force: bool = False)")
            ),
        );
        let context = repo.run_json("context", &["--git"]);
        let file = changed_file(&context, "app/protocols.py");
        assert_change(&file, "Counter", "class", &["modified"]);
        assert!(
            file["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["path"] == "app/consumer.py"),
            "{file}"
        );
    }
}

#[test]
fn python_added_class_keeps_cross_file_moves_conservative() {
    let repo = protocols_repo("");
    repo.write(
        "app/previous.py",
        format!("from typing import Protocol\n{COUNTER}"),
    );
    repo.commit_all("old owner");
    repo.write("app/previous.py", "");
    repo.write(
        "app/protocols.py",
        format!(
            "{}{COUNTER}",
            PROTOCOLS.replace("flush(self)", "flush(self, force: bool = False)")
        ),
    );
    let context = repo.run_json("context", &["--git"]);
    let file = changed_file(&context, "app/protocols.py");
    assert_change(&file, "Counter", "class", &["modified"]);
}

#[test]
fn python_changed_method_keeps_same_file_competing_definitions_and_unknown_receivers() {
    for added_bundle in [false, true] {
        let repo = trail_repo();
        repo.write("routes/consumer.py", "from routes.trail import _Trail\n\ndef use():\n    trail = _Trail()\n    def join(part):\n        return part\n    return trail.join(\"part\")\n\ndef unknown(value):\n    return relay(value).join(\"part\")\n");
        repo.commit_all("actual and unknown consumers");
        let changed = if added_bundle {
            TRAIL_CHANGED.to_owned()
        } else {
            TRAIL
                .replace("def join(self, part):", "def join(self, part, origin):")
                .replace(
                    "return self.prefix + part",
                    "return origin + self.prefix + part",
                )
                .replace("trail.join(part)", "trail.join(part, \"local\")")
        };
        repo.write("routes/trail.py", changed);
        let context = repo.run_json("context", &["--git"]);
        let file = changed_file(&context, "routes/trail.py");
        let callers = file["impacted_callers"].as_array().unwrap();
        assert_eq!(callers.len(), 2, "{file}");
        assert!(
            callers
                .iter()
                .all(|row| row["path"] == "routes/consumer.py"),
            "{file}"
        );
        let output = cargo_bin()
            .args(["impact", "--dir"])
            .arg(repo.root())
            .arg("--git")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
    }
}

#[test]
fn python_changed_method_keeps_local_module_aliases_blocking() {
    let repo = trail_repo();
    repo.write(
        "helper.py",
        "from routes.trail import _Trail\njoin = _Trail.join\n",
    );
    repo.write(
        "routes/consumer.py",
        "import helper\n\ndef use(value):\n    return helper.join(value, \"part\")\n",
    );
    repo.commit_all("module alias consumer");
    repo.write("routes/trail.py", TRAIL_CHANGED);
    let context = repo.run_json("context", &["--git"]);
    let file = changed_file(&context, "routes/trail.py");
    assert!(
        file["impacted_callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == "routes/consumer.py"),
        "{file}"
    );
}

#[test]
fn python_changed_method_keeps_src_layout_module_aliases_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("trail.py", TRAIL.replace("_Trail", "Trail"));
    repo.write(
        "src/helper.py",
        "from trail import Trail\njoin = Trail.join\n",
    );
    repo.write(
        "consumer.py",
        "import helper\n\ndef use(value):\n    return helper.join(value, \"part\")\n",
    );
    repo.write("pyproject.toml", "[project]\nname = \"src-alias-fixture\"\nversion = \"0.0.0\"\n\n[tool.setuptools]\npackage-dir = {\"\" = \"src\"}\npy-modules = [\"helper\"]\n");
    repo.init_git();
    repo.commit_all("src layout module alias");
    repo.write("trail.py", TRAIL_CHANGED.replace("_Trail", "Trail"));
    let context = repo.run_json("context", &["--git"]);
    let file = changed_file(&context, "trail.py");
    let callers = file["impacted_callers"].as_array().unwrap();
    for path in ["consumer.py", "src/helper.py"] {
        assert!(callers.iter().any(|row| row["path"] == path), "{file}");
    }
}

#[test]
fn python_changed_method_keeps_custom_packaging_roots_unproven() {
    let repo = TestRepo::new();
    repo.create_dir_all("lib");
    repo.write("trail.py", TRAIL.replace("_Trail", "Trail"));
    repo.write(
        "lib/helper.py",
        "from trail import Trail\njoin = Trail.join\n",
    );
    repo.write(
        "consumer.py",
        "import helper\n\ndef use(value):\n    return helper.join(value, \"part\")\n",
    );
    repo.write("pyproject.toml", "[project]\nname = \"custom-root-fixture\"\nversion = \"0.0.0\"\n\n[tool.setuptools]\npackage-dir = {\"\" = \"lib\"}\npy-modules = [\"helper\"]\n");
    repo.init_git();
    repo.commit_all("custom packaging root");
    repo.write("trail.py", TRAIL_CHANGED.replace("_Trail", "Trail"));
    let context = repo.run_json("context", &["--git"]);
    let file = changed_file(&context, "trail.py");
    let callers = file["impacted_callers"].as_array().unwrap();
    for path in ["consumer.py", "lib/helper.py"] {
        assert!(callers.iter().any(|row| row["path"] == path), "{file}");
    }
}

#[test]
fn python_added_method_staged_review_uses_snapshot_binding_evidence() {
    let repo = trail_repo();
    repo.write("routes/trail.py", TRAIL_CHANGED);
    repo.stage_all();
    repo.write("routes/trail.py", TRAIL);
    repo.write(
        "routes/labels.py",
        format!("{LABELS}\n\ndef unknown(value):\n    return value.join(\"part\")\n"),
    );
    let review = repo.run_json("review", &["--git", "--staged"]);
    let file = changed_file(&review["impact"], "routes/trail.py");
    assert!(
        file["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{review}"
    );
    assert_change(&file, "_Bundle", "class", &["added"]);
    let direct = repo.run_json("context", &["--git", "--staged"]);
    let file = changed_file(&direct, "routes/trail.py");
    assert!(
        file["impacted_callers"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["path"] == "routes/labels.py")),
        "{direct}"
    );
}
