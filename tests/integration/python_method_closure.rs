//! 同じ差分で追随したメソッドと、無関係な同名の呼び出しを区別する。

use super::support::*;

const BEFORE: &str = "class _Trail:\n    prefix = \"/\"\n\n    def join(self, part):\n        return self.prefix + part\n\n\ndef collect(parts):\n    trail = _Trail()\n    return [trail.join(part) for part in parts]\n";
const STRING: &str = "def label(words):\n    return \", \".join(words)\n";
const MODULE: &str =
    "import os.path\n\ndef location(root, name):\n    return os.path.join(root, name)\n";
const LOCAL: &str = "def chain(words):\n    out = []\n    def join(word):\n        out.append(word.strip())\n    for word in words:\n        join(word)\n    return out\n";

fn method_repo(owner: &str, method: &str, labels: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("routes");
    repo.write("routes/__init__.py", "");
    repo.write(
        "routes/trail.py",
        BEFORE.replace("_Trail", owner).replace("join", method),
    );
    repo.write("routes/labels.py", labels);
    repo.init_git();
    repo.commit_all("initial");
    repo
}

fn updated(owner: &str, method: &str) -> String {
    BEFORE
        .replace("_Trail", owner)
        .replace("join(self, part)", "join(self, part, origin)")
        .replace(
            "return self.prefix + part",
            "return origin + self.prefix + part",
        )
        .replace("trail.join(part)", "trail.join(part, \"local\")")
        .replace("join", method)
}

fn commit_opaque_index_entry(repo: &TestRepo, path: &str, mode: &str) {
    let output = if mode == "160000" {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.root())
            .output()
            .unwrap()
    } else {
        repo.write("opaque-link.txt", "../outside.py");
        std::process::Command::new("git")
            .args(["hash-object", "-w", "opaque-link.txt"])
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

fn hook(repo: &TestRepo, staged: bool) -> (Option<i32>, serde_json::Value) {
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let json = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("invalid hook JSON: {error}: {output:?}"));
    (output.status.code(), json)
}

fn assert_closed(repo: &TestRepo, name: &str, staged: bool) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let review = repo.run_json("review", args);
    assert!(
        review["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .all(|change| change["name"] != name),
        "{review}"
    );
    let (code, json) = hook(repo, staged);
    assert_eq!(code, Some(0), "{name}/{staged}: {json}");
    assert_eq!(json["blocking_categories"], serde_json::json!([]), "{json}");
}

fn assert_blocked(repo: &TestRepo, name: &str, staged: bool) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let review = repo.run_json("review", args);
    assert!(
        review["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["name"] == name),
        "{review}"
    );
    let (code, json) = hook(repo, staged);
    assert_eq!(code, Some(1), "{name}/{staged}: {json}");
    assert!(
        json["blocking_categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category == "api.mod"),
        "{json}"
    );
}

fn labels_refs(repo: &TestRepo, method: &str) -> Vec<serde_json::Value> {
    repo.run_json("refs", &["--name", method])["refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["path"] == "routes/labels.py")
        .cloned()
        .collect()
}

fn check_positive(labels: &str, method: &str) {
    for owner in ["_Trail", "Trail"] {
        for staged in [false, true] {
            let repo = method_repo(owner, method, labels);
            let before = labels_refs(&repo, method);
            assert!(!before.is_empty());
            repo.write("routes/trail.py", updated(owner, method));
            if staged {
                repo.stage_all();
            }
            assert_closed(&repo, &format!("{owner}.{method}"), staged);
            assert_eq!(before, labels_refs(&repo, method));
        }
    }
}

#[test]
fn python_method_closure_ignores_literal_string_calls() {
    check_positive(STRING, "join");
}

#[test]
fn python_method_closure_ignores_external_module_calls() {
    check_positive(MODULE, "join");
}

#[test]
fn python_method_closure_ignores_lexical_local_function_calls() {
    check_positive(LOCAL, "join");
}

#[test]
fn python_method_closure_ignores_single_literal_list_bindings() {
    for assignment in ["rows = []", "rows: list[str] = []"] {
        let labels = format!(
            "def tidy(words):\n    {assignment}\n    for word in words:\n        rows.append(word.strip())\n    return rows\n"
        );
        check_positive(&labels, "append");
    }
}

#[test]
fn python_method_closure_combines_static_forms_without_a_name_allowlist() {
    check_positive(&format!("{STRING}\n{MODULE}\n{LOCAL}"), "join");
    check_positive(&LOCAL.replace("join", "stitch"), "stitch");
}

#[test]
fn python_method_closure_shares_static_reference_facts_across_changed_methods() {
    let local = "def chain(words):\n    def join(word):\n        return word.strip()\n    return [join(word) for word in words]\n";
    let list = "def tidy(words):\n    rows = []\n    for word in words:\n        rows.append(word.strip())\n    return rows\n";
    let labels = format!("{STRING}\n{MODULE}\n{local}\n{list}");
    let repo = method_repo("_Trail", "join", &labels);
    let before = BEFORE
        .replace(
            "\n\ndef collect",
            "\n    def append(self, part):\n        return self.prefix + part\n\n\ndef collect",
        )
        .replace("trail.join(part)", "trail.join(part) + trail.append(part)");
    repo.write("routes/trail.py", &before);
    repo.commit_all("two methods");
    let join_refs = labels_refs(&repo, "join");
    let append_refs = labels_refs(&repo, "append");
    repo.write(
        "routes/trail.py",
        before
            .replace("(self, part)", "(self, part, origin)")
            .replace("trail.join(part)", "trail.join(part, 'local')")
            .replace("trail.append(part)", "trail.append(part, 'local')"),
    );
    assert_closed(&repo, "_Trail.join", false);
    assert_eq!(join_refs, labels_refs(&repo, "join"));
    assert_eq!(append_refs, labels_refs(&repo, "append"));
}

#[test]
fn python_method_closure_keeps_unknown_and_stale_callers_blocking() {
    for labels in [
        "def relay(trail, part):\n    return trail.join(part)\n".to_owned(),
        format!(
            "{LOCAL}\nfrom routes.trail import _Trail\n\ndef relay(part):\n    trail = _Trail()\n    return trail.join(part)\n"
        ),
        "def relay(trail, part):\n    join = trail.join\n    return join(part)\n".to_owned(),
    ] {
        let repo = method_repo("_Trail", "join", &labels);
        repo.write("routes/trail.py", updated("_Trail", "join"));
        assert_blocked(&repo, "_Trail.join", false);
    }
    for arguments in ["part", "part, 'local', 'extra'"] {
        let repo = method_repo("_Trail", "join", STRING);
        repo.write(
            "routes/trail.py",
            updated("_Trail", "join").replace("part, \"local\"", arguments),
        );
        assert_blocked(&repo, "_Trail.join", false);
    }
}

#[test]
fn python_method_closure_keeps_local_module_aliases_and_custom_roots_blocking() {
    for (directory, configuration) in [("", None), ("src/", None), ("lib/", Some("pyproject.toml"))]
    {
        let repo = method_repo(
            "_Trail",
            "join",
            "import helper\n\ndef relay(part):\n    return helper.join(part)\n",
        );
        if !directory.is_empty() {
            repo.create_dir_all(directory);
        }
        repo.write(
            format!("{directory}helper.py"),
            "from routes.trail import _Trail\njoin = _Trail().join\n",
        );
        if let Some(configuration) = configuration {
            repo.write(configuration, "[tool.example]\nsource = 'lib'\n");
        }
        repo.commit_all("local module alias");
        repo.write("routes/trail.py", updated("_Trail", "join"));
        assert_blocked(&repo, "_Trail.join", false);
    }
}

#[test]
fn python_method_closure_keeps_rebound_list_receivers_blocking() {
    for labels in [
        "def tidy(values):\n    rows = []\n    rows = values\n    return rows.append('x')\n",
        "def tidy(rows):\n    rows = []\n    return rows.append('x')\n",
        "def tidy(values):\n    rows = []\n    for rows in values:\n        rows.append('x')\n",
        "def tidy(values):\n    rows = []\n    return [rows.append('x') for rows in values]\n",
        "async def tidy(values):\n    rows = []\n    return [rows.append('x') async for rows in values]\n",
        "def tidy(values):\n    rows = []\n    return [[rows.append('x') for rows in values] for value in values]\n",
        "def tidy(values):\n    global rows\n    rows = []\n    return rows.append('x')\n",
        "def outer(values):\n    rows = []\n    def tidy():\n        nonlocal rows\n        rows = []\n        return rows.append('x')\n    return tidy()\n",
        "def tidy(enabled):\n    if enabled:\n        rows = []\n    return rows.append('x')\n",
        "def tidy(values):\n    rows = []\n    exec(values)\n    return rows.append('x')\n",
    ] {
        let repo = method_repo("_Trail", "append", labels);
        repo.write("routes/trail.py", updated("_Trail", "append"));
        assert_blocked(&repo, "_Trail.append", false);
    }
}

#[test]
fn python_method_closure_keeps_comprehension_module_shadows_blocking() {
    for expression in [
        "[os.path.join('x') for os in values]",
        "[[os.path.join('x') for os in values] for item in values]",
        "[os.path.join('x') for item in values for os in values]",
        "[os.path.join('x') async for os in values]",
    ] {
        let labels =
            format!("import os.path\n\nasync def relay(values):\n    return {expression}\n");
        let repo = method_repo("_Trail", "join", &labels);
        repo.write("routes/trail.py", updated("_Trail", "join"));
        assert_blocked(&repo, "_Trail.join", false);
    }
}

#[test]
fn python_method_closure_requires_the_updated_comprehension_receiver_binding() {
    for source in [
        updated("_Trail", "join").replace("for part in parts", "for trail in parts"),
        updated("_Trail", "join")
            .replace("for part in parts", "for part in parts for trail in parts"),
        updated("_Trail", "join")
            .replace("def collect", "async def collect")
            .replace("for part in parts", "async for trail in parts"),
        updated("_Trail", "join").replace("def collect(parts)", "def collect(parts, _Trail)"),
        updated("_Trail", "join").replace(
            "return [trail.join(part, \"local\") for part in parts]",
            "return lambda part: trail.join(part, \"local\")",
        ),
    ] {
        let repo = method_repo("_Trail", "join", STRING);
        repo.write("routes/trail.py", source);
        assert_blocked(&repo, "_Trail.join", false);
    }
}

#[test]
fn python_method_closure_uses_index_binding_proofs_when_worktree_diverges() {
    let unknown = "def relay(trail, part):\n    return trail.join(part)\n";
    let valid_index = method_repo("_Trail", "join", &format!("{STRING}\n{MODULE}\n{LOCAL}"));
    valid_index.write("routes/trail.py", updated("_Trail", "join"));
    valid_index.stage_all();
    valid_index.write("routes/labels.py", unknown);
    assert_closed(&valid_index, "_Trail.join", true);
    assert_blocked(&valid_index, "_Trail.join", false);

    let invalid_index = method_repo("_Trail", "join", unknown);
    invalid_index.write("routes/trail.py", updated("_Trail", "join"));
    invalid_index.stage_all();
    invalid_index.write("routes/labels.py", STRING);
    assert_closed(&invalid_index, "_Trail.join", false);
    assert_blocked(&invalid_index, "_Trail.join", true);
}

#[test]
fn python_method_closure_keeps_dynamic_and_incomplete_source_evidence_blocking() {
    for labels in [
        format!("import os.path\nos.path = other\n{MODULE}"),
        format!("{LOCAL}\nexec(code)\n"),
        format!("#{}\n{STRING}", "x".repeat(1024 * 1024)),
        format!("{STRING}\nif:\n"),
    ] {
        let repo = method_repo("_Trail", "join", &labels);
        repo.write("routes/trail.py", updated("_Trail", "join"));
        assert_blocked(&repo, "_Trail.join", false);
    }
}

#[test]
fn python_method_closure_staged_namespace_proof_keeps_omitted_index_entries_blocking() {
    for (path, mode) in [
        ("helper.py", "120000"),
        ("helper/__init__.py", "120000"),
        ("custom.pth", "120000"),
        ("helper", "160000"),
    ] {
        let repo = method_repo(
            "_Trail",
            "join",
            "import helper\n\ndef relay(part):\n    return helper.join(part)\n",
        );
        commit_opaque_index_entry(&repo, path, mode);
        repo.write("routes/trail.py", updated("_Trail", "join"));
        repo.git(["add", "routes/trail.py"]);
        assert_blocked(&repo, "_Trail.join", true);
    }
}

#[test]
fn python_method_closure_staged_opaque_paths_preserve_other_static_proofs() {
    for (method, labels) in [
        ("join", format!("{STRING}\n{LOCAL}")),
        ("append", "def tidy(words):\n    rows = []\n    for word in words:\n        rows.append(word)\n    return rows\n".to_owned()),
    ] {
        let repo = method_repo("_Trail", method, &labels);
        commit_opaque_index_entry(&repo, "helper.py", "120000");
        repo.write("routes/trail.py", updated("_Trail", method));
        repo.git(["add", "routes/trail.py"]);
        assert_closed(&repo, &format!("_Trail.{method}"), true);
    }
}
