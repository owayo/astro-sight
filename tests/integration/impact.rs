//! impact サブコマンド (変更後の未解決影響検出) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn impact_python_relative_import_belongs_to_its_defining_module() {
    for consumer in [
        "from .cli import main\nraise SystemExit(main())\n",
        "from .cli import main\ndef run():\n    return main()\n",
        "from .cli import (main,)\ncallback = main\n",
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("pkg");
        repo.write("legacy.py", "def main():\n    return 0\n");
        repo.write("pkg/__init__.py", "");
        repo.write(
            "pkg/cli.py",
            "def main() -> int:\n    return 0\n\ndef helper():\n    return 0\n",
        );
        repo.write("pkg/__main__.py", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "legacy.py",
            "def main(required: str) -> int:\n    return 0\n",
        );

        let context = repo.run_json("context", &["--git"]);
        assert_eq!(context["changes"].as_array().unwrap().len(), 1);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{consumer}: {context}"
        );
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{consumer}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // 同じ参照で実際の所有元を変更すると未解決影響になる。
        repo.write("pkg/cli.py", "def main(required: str) -> int:\n    return 0\n\ndef helper(required: str):\n    return 0\n");
        let context = repo.run_json("context", &["--git"]);
        let changes = context["changes"].as_array().unwrap();
        let legacy = changes.iter().find(|c| c["path"] == "legacy.py").unwrap();
        assert!(
            legacy["impacted_callers"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{context}"
        );
        let actual = changes.iter().find(|c| c["path"] == "pkg/cli.py").unwrap();
        assert!(
            actual["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == "pkg/__main__.py"),
            "{context}"
        );
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{consumer}");
    }
}

#[test]
fn impact_python_import_proof_keeps_unresolved_and_ambiguous_callers() {
    for (target, consumer) in [
        (None, "from .cli import main\nmain()\n"),
        (
            Some("from legacy import main\n"),
            "from .cli import main\nmain()\n",
        ),
        (
            Some("if True:\n    def main():\n        return 0\n"),
            "from .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\nmain = other\n"),
            "from .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\ndef broken(\n"),
            "from .cli import main\nmain()\n",
        ),
        (
            Some(
                "import legacy\ndef __getattr__(name):\n    return legacy.main\nimport pkg.runner\ndef main():\n    return 0\n",
            ),
            "from .cli import main\nmain()\n",
        ),
        (
            Some("import legacy\n@decorate(legacy.main)\ndef main():\n    return 0\n"),
            "from .cli import main\nmain()\n",
        ),
        (
            Some(
                "import legacy\ndef main():\n    return 0\nmain.__globals__.update(main=legacy.main)\n",
            ),
            "from .cli import main\nmain()\n",
        ),
        (
            Some(
                "import sys, legacy\ndef main():\n    return 0\nsys.modules[__name__].__dict__.update(main=legacy.main)\n",
            ),
            "from .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nimport sys, legacy\nsys._getframe().f_globals.update(main=legacy.main)\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nimport sys, legacy\nsys.modules[__name__].__setattr__('main', legacy.main)\nmain()\n",
        ),
        (
            Some(
                "import sys, legacy\ndef main():\n    return 0\nsys.modules.update({__name__: legacy})\n",
            ),
            "from .cli import main\nmain()\n",
        ),
        (
            Some(
                "import sys, legacy\ndef main():\n    return 0\nsys.modules[__spec__.name] = legacy\n",
            ),
            "from .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "import sys, legacy\nsys.modules['pkg.cli'] = legacy\nfrom .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "import types, legacy, pkg.cli\nclass M(types.ModuleType):\n    def __getattribute__(self, name):\n        return getattr(legacy, name)\npkg.cli.__class__ = M\nfrom .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nimport legacy\ng = globals\ng().update(main=legacy.main)\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "import other.sub\n__spec__ = other.sub.__spec__\nfrom .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nfrom legacy import *\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nmain = other\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\ndef run():\n    global main\n    return main()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nobj.main()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nexec(code)\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "main()\nfrom .cli import main\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "if True:\n    from .cli import main\nmain()\n",
        ),
        (
            Some("def main():\n    return 0\n"),
            "from .cli import main\nimport legacy\nmain(); legacy.main()\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("pkg");
        repo.write("legacy.py", "def main():\n    return 0\n");
        repo.write("pkg/__init__.py", "");
        if let Some(target) = target {
            repo.write("pkg/cli.py", target);
        }
        repo.write("pkg/runner.py", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("legacy.py", "def main(required):\n    return 0\n");
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == "pkg/runner.py"),
            "{target:?} / {consumer}: {context}"
        );
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{consumer}");
    }
}

#[test]
fn impact_python_import_keeps_type_stub_changes() {
    for stub in ["pkg/cli.pyi", "pkg/cli.PYI"] {
        let repo = TestRepo::new();
        repo.create_dir_all("pkg");
        repo.write("pkg/__init__.py", "");
        repo.write("pkg/cli.py", "def main():\n    return 0\n");
        repo.write(stub, "def main() -> int: ...\n");
        repo.write("pkg/runner.py", "from .cli import main\nmain()\n");
        repo.init_git();
        repo.commit_all("initial");
        repo.write(stub, "def main(required: str) -> int: ...\n");
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("pkg/runner.py"));
    }
}

#[test]
#[cfg(unix)]
fn impact_python_import_does_not_hide_same_source_through_symlink() {
    let repo = TestRepo::new();
    repo.create_dir_all("pkg");
    repo.write("legacy.py", "def main():\n    return 0\n");
    repo.write("pkg/__init__.py", "");
    std::os::unix::fs::symlink("../legacy.py", repo.path("pkg/cli.py")).unwrap();
    repo.write("pkg/runner.py", "from .cli import main\nmain()\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write("legacy.py", "def main(required):\n    return 0\n");
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("pkg/runner.py"));
}

#[test]
fn impact_python_local_bindings_are_not_external_callers() {
    for consumer in [
        "def main():\n    collect_items = []\n    collect_items.append(1)\n    return len(collect_items)\n",
        "def main(collect_items):\n    return collect_items\n",
        "def main(collect_items: list):\n    return collect_items\n",
        "def main(collect_items=[]):\n    return collect_items\n",
        "def main(collect_items: list=[]):\n    return collect_items\n",
        "def main():\n    collect_items: list[int] = []\n    return len(collect_items)\n",
        "def helper(value: dict[str, int]):\n    pass\ndef main():\n    collect_items = []\n    return len(collect_items)\n",
        "def main():\n    for collect_items in [[]]:\n        print(collect_items)\n",
        "from producer import collect_items\ndef main():\n    collect_items = []\n    return collect_items\n",
    ] {
        let repo = TestRepo::new();
        repo.write(
            "producer.py",
            "def collect_items(limit):\n    return list(range(limit))\n",
        );
        repo.write("runner.py", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("producer.py", "def collect_items(limit, reverse=False):\n    values = list(range(limit))\n    return values[::-1] if reverse else values\n");
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{consumer}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn impact_python_keeps_imported_and_ambiguous_callers() {
    for consumer in [
        "from producer import collect_items\ndef main():\n    return collect_items(3)\n",
        "import producer\ndef main(collect_items):\n    return producer.collect_items(3)\n",
        "def main():\n    from producer import collect_items\n    return collect_items(3)\n",
        "from producer import collect_items\ndef main():\n    global collect_items\n    collect_items = collect_items(3)\n",
        "from producer import collect_items\ndef main(collect_items=collect_items(3)):\n    return collect_items\n",
        "from producer import collect_items\ndef main():\n    def inner():\n        return collect_items(3)\n    return inner()\n",
    ] {
        let repo = TestRepo::new();
        repo.write(
            "producer.py",
            "def collect_items(limit):\n    return list(range(limit))\n",
        );
        repo.write("runner.py", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "producer.py",
            "def collect_items(limit, required):\n    return list(range(limit))\n",
        );
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains("runner.py"),
            "{consumer}: {stderr}"
        );
    }
}

#[test]
fn impact_python_signature_reformat_keeps_real_changes() {
    let repo = TestRepo::new();
    repo.write("api.py", "def formatted(\n    value: str = 'a  b',\n) -> str:\n    return value\n\ndef changed(value=(1,)):\n    return value\n");
    repo.write(
        "consumer.py",
        "from api import formatted, changed\nformatted()\nchanged()\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    for value in ["(1)", "'a  b'", "'a b'"] {
        repo.write("api.py", format!("def formatted(value: str = 'a  b') -> str:\n    return value\n\ndef changed(value={value}):\n    return value\n"));
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(
            stderr.contains("changed") && stderr.contains("consumer.py"),
            "{stderr}"
        );
        assert!(!stderr.contains("formatted"), "{stderr}");
        // 次の変更では文字列内部の空白だけが異なる対照になる。
        repo.commit_all("change default");
    }
}

#[test]
fn impact_rust_type_alias_rhs_change_blocks_cross_file_callers() {
    for visibility in ["", "pub(crate) ", "pub "] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "src/lib.rs",
            format!("mod consumer;\n\n{visibility}type Id = u32;\n"),
        );
        repo.write(
            "src/consumer.rs",
            "use crate::Id;\npub fn read() -> Id { 1_u32 }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/lib.rs",
            format!("mod consumer;\n\n{visibility}type Id = u64;\n"),
        );

        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/lib.rs")
            .expect("alias change should be visible");
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers.iter().any(|caller| {
                    caller["path"] == "src/consumer.rs"
                        && caller["symbols"]
                            .as_array()
                            .is_some_and(|names| names.iter().any(|name| name == "Id"))
                })),
            "{visibility}: {context}"
        );

        for command in ["impact", "review"] {
            let output = cargo_bin()
                .args([
                    command,
                    "--dir",
                    repo.root().to_str().unwrap(),
                    "--git",
                    "--hook",
                ])
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(1),
                "{visibility} {command}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        repo.write(
            "src/consumer.rs",
            "use crate::Id;\npub fn read() -> Id { 1_u64 }\n",
        );
        let output = cargo_bin()
            .args([
                "impact",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{visibility}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn impact_rust_type_alias_requires_a_contract_change() {
    for (old, new, blocking) in [
        (
            "pub type Id = u32; // old\n",
            "pub type Id = u32; // new\n",
            false,
        ),
        (
            "pub type Id =\n    u32;\n",
            "pub type Id =\n    u64;\n",
            true,
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write("src/lib.rs", format!("mod consumer;\n{old}"));
        repo.write(
            "src/consumer.rs",
            "use crate::Id;\npub fn read() -> Id { 1_u32 }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/lib.rs", format!("mod consumer;\n{new}"));

        let context = repo.run_json("context", &["--git"]);
        let impacted = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/lib.rs")
            .and_then(|change| change["impacted_callers"].as_array())
            .is_some_and(|callers| {
                callers
                    .iter()
                    .any(|caller| caller["path"] == "src/consumer.rs")
            });
        assert_eq!(impacted, blocking, "{context}");
        let output = cargo_bin()
            .args([
                "impact",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(i32::from(blocking)),
            "{context}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn impact_rust_free_function_excludes_same_named_method_calls() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("src/lib.rs", "pub mod walk;\nmod stats;\nmod caller;\n");
    repo.write(
        "src/walk.rs",
        "pub fn collect() -> (usize, usize) { (0, 0) }\n",
    );
    repo.write(
        "src/stats.rs",
        "pub fn values() -> Vec<i32> { vec![1].into_iter().collect() }\n",
    );
    repo.write(
        "src/caller.rs",
        "pub fn run() -> (usize, usize) { crate::walk::collect() }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/walk.rs",
        "pub struct Found { pub count: usize }\npub fn collect() -> Found { Found { count: 0 } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/walk.rs")
        .unwrap();
    let callers = change["impacted_callers"]
        .as_array()
        .unwrap_or_else(|| panic!("{context}"));
    assert!(
        callers
            .iter()
            .any(|caller| caller["path"] == "src/caller.rs"),
        "{context}"
    );
    assert!(
        callers
            .iter()
            .all(|caller| caller["path"] != "src/stats.rs"),
        "{context}"
    );
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));

    repo.write(
        "src/caller.rs",
        "pub fn run() -> usize { crate::walk::collect().count }\n",
    );
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn impact_rust_changed_method_keeps_receiver_calls() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("src/lib.rs", "pub mod walk;\nmod consumer;\n");
    repo.write(
        "src/walk.rs",
        "pub struct Collector;\nimpl Collector { pub fn collect(&self) -> usize { 0 } }\n",
    );
    repo.write(
        "src/consumer.rs",
        "use crate::walk::Collector;\npub fn run() -> usize { Collector.collect() }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/walk.rs",
        "pub struct Collector;\nimpl Collector { pub fn collect(&self, value: usize) -> usize { value } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/walk.rs")
        .unwrap();
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/consumer.rs")),
        "{context}"
    );
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn impact_rust_std_import_does_not_claim_local_type_change() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[workspace]\n[workspace.package]\nedition = \"2021\"\n[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod out;\nmod other;\nmod real;\nmod mixed;\n",
    );
    repo.write(
        "src/out.rs",
        "pub struct Range { pub start: usize, pub end: usize }\n",
    );
    repo.write(
        "src/other.rs",
        "use std::ops::Range;\n#[derive(Debug)]\nstruct Holder;\n#[cfg(test)]\nmod tests {}\npub fn len(r: Range<usize>) -> usize { r.end - r.start }\npub fn explicit(r: std::ops::Range<usize>) -> usize { r.end - r.start }\n",
    );
    repo.write(
        "src/real.rs",
        "use crate::out::Range;\npub fn end(r: Range) -> usize { r.end }\n",
    );
    repo.write(
        "src/mixed.rs",
        "use std::ops::Range;\npub fn local(r: crate::out::Range) -> usize { r.end }\npub fn standard(r: Range<usize>) -> usize { r.end }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/out.rs",
        "pub struct Range { pub start: usize, pub end: u64 }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/out.rs")
        .unwrap();
    let callers = change["impacted_callers"].as_array().unwrap();
    assert!(
        callers.iter().any(|caller| caller["path"] == "src/real.rs"),
        "{context}"
    );
    assert!(
        callers
            .iter()
            .any(|caller| caller["path"] == "src/mixed.rs" && caller["name"] == "local"),
        "{context}"
    );
    assert!(
        callers
            .iter()
            .all(|caller| caller["path"] != "src/other.rs"),
        "{context}"
    );
    assert!(
        callers
            .iter()
            .all(|caller| { caller["path"] != "src/mixed.rs" || caller["name"] == "local" }),
        "{context}"
    );
    assert!(
        change["informational_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .all(|caller| caller["path"] != "src/other.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_std_result_keeps_local_alias_usage() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod out;\nmod other;\nmod real;\n");
    repo.write(
        "src/out.rs",
        "pub type Result<T> = std::result::Result<T, ()>;\n",
    );
    repo.write(
        "src/other.rs",
        "use std::io::Result;\nfn standard(value: Result<u64>) -> std::io::Result<u64> { value }\n",
    );
    repo.write(
        "src/real.rs",
        "use crate::out::Result;\nfn local(value: Result<u64>) -> Result<u64> { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/out.rs",
        "pub type Result<T> = std::result::Result<T, String>;\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/out.rs")
        .unwrap();
    let callers = change["impacted_callers"].as_array().unwrap();
    assert!(
        callers.iter().any(|caller| caller["path"] == "src/real.rs"),
        "{context}"
    );
    assert!(
        callers
            .iter()
            .all(|caller| caller["path"] != "src/other.rs"),
        "{context}"
    );
}

#[test]
fn impact_rust_cargo_target_roots_keep_private_alias_callers() {
    for (root_path, target_section) in [
        ("src/bin/tool.rs", ""),
        (
            "tools/tool.rs",
            "[[bin]]\nname = \"tool\"\npath = \"tools/tool.rs\"\n",
        ),
        ("core/lib.rs", "[lib]\npath = \"core/lib.rs\"\n"),
    ] {
        let repo = TestRepo::new();
        let root_dir = std::path::Path::new(root_path).parent().unwrap();
        repo.create_dir_all(root_dir.join("consumer"));
        repo.write(
            "Cargo.toml",
            format!(
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{target_section}"
            ),
        );
        repo.write(
            root_path,
            "mod consumer;\ntype Result<T> = std::result::Result<T, ()>;\nfn main() {}\n",
        );
        let consumer_path = root_dir.join("consumer/mod.rs");
        let expected_consumer_path = consumer_path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        repo.write(
            &consumer_path,
            "fn caller(value: super::Result<u8>) -> super::Result<u8> { value }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            root_path,
            "mod consumer;\ntype Result<T> = std::result::Result<T, String>;\nfn main() {}\n",
        );

        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == root_path)
            .unwrap();
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| { caller["path"] == expected_consumer_path })),
            "root={root_path}: {context}"
        );
    }
}

#[test]
fn impact_rust_custom_target_root_uses_index_manifest() {
    let repo = TestRepo::new();
    repo.create_dir_all("tools/consumer");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[[bin]]\nname = \"tool\"\npath = \"tools/tool.rs\"\n",
    );
    repo.write(
        "tools/tool.rs",
        "mod consumer;\ntype Result<T> = std::result::Result<T, ()>;\nfn main() {}\n",
    );
    repo.write(
        "tools/consumer/mod.rs",
        "fn caller(value: super::Result<u8>) -> super::Result<u8> { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "tools/tool.rs",
        "mod consumer;\ntype Result<T> = std::result::Result<T, String>;\nfn main() {}\n",
    );
    repo.stage_all();
    // Working tree の manifest は index と異なる。staged 側の対象を読む必要がある。
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[[bin]]\nname = \"other\"\npath = \"tools/other.rs\"\n",
    );

    let context = repo.run_json("context", &["--git", "--staged"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "tools/consumer/mod.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_auto_root_keeps_caller_with_unreadable_manifest() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/bin/consumer");
    repo.write("Cargo.toml", "[package\ninvalid");
    repo.write(
        "src/bin/tool.rs",
        "mod consumer;\ntype Result<T> = std::result::Result<T, ()>;\nfn main() {}\n",
    );
    repo.write(
        "src/bin/consumer/mod.rs",
        "fn caller(value: super::Result<u8>) -> super::Result<u8> { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/bin/tool.rs",
        "mod consumer;\ntype Result<T> = std::result::Result<T, String>;\nfn main() {}\n",
    );

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/bin/consumer/mod.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_regular_module_is_not_cargo_target_root() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod model;\nmod consumer;\n");
    repo.write("src/model.rs", "type Payload = u8;\n");
    repo.write(
        "src/consumer.rs",
        "type Payload = u16;\nfn caller(value: Payload) -> Payload { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/model.rs", "type Payload = u32;\n");

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{context}"
    );
}

#[test]
fn impact_rust_paths_keep_only_their_module_owner() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/other");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin;\npub mod other;\npub mod real;\npub mod unrelated;\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write("src/unrelated.rs", "#[derive(Debug)] pub struct Helper;\n");
    repo.write(
        "src/other.rs",
        "use std::fmt::Debug;\npub struct Payload { pub other: u8 }\nmod child;\npub fn local(value: Payload) -> Payload { value }\npub fn self_path(value: self::Payload) -> self::Payload { value }\npub fn explicit(value: crate::other::Payload) -> crate::other::Payload { value }\n",
    );
    repo.write(
        "src/other/child.rs",
        "pub fn parent(value: super::Payload) -> super::Payload { value }\n",
    );
    repo.write(
        "src/real.rs",
        "use crate::origin::Payload;\npub fn caller(value: Payload) -> Payload { value }\npub fn explicit(value: crate::origin::Payload) -> crate::origin::Payload { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == "src/real.rs")),
        "{context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket]
                .as_array()
                .is_none_or(|callers| callers.iter().all(|caller| {
                    caller["path"] != "src/other.rs" && caller["path"] != "src/other/child.rs"
                })),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_local_type_path_is_not_another_modules_free_function() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin;\npub mod other;\npub mod real;\n",
    );
    repo.write("src/origin.rs", "pub fn create() -> u8 { 0 }\n");
    repo.write(
        "src/other.rs",
        "pub struct Factory;\nimpl Factory { pub fn create() -> u8 { 1 } }\npub fn caller() -> u8 { Factory::create() }\n",
    );
    repo.write(
        "src/real.rs",
        "pub fn caller() -> u8 { crate::origin::create() }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/origin.rs",
        "pub fn create(value: u8) -> u8 { value }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == "src/real.rs")),
        "{context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(|callers| callers
                .iter()
                .all(|caller| caller["path"] != "src/other.rs")),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_path_owner_keeps_reexports_and_path_attributes() {
    for (root_source, bridge_source) in [
        (
            "pub mod origin;\npub mod bridge;\npub mod consumer;\n",
            "pub use crate::origin::Payload;\n",
        ),
        (
            "#[path = \"origin.rs\"] pub mod bridge;\npub mod consumer;\n",
            "pub struct Payload { pub other: u8 }\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", root_source);
        repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
        repo.write("src/bridge.rs", bridge_source);
        repo.write(
            "src/consumer.rs",
            "pub fn caller(value: crate::bridge::Payload) -> crate::bridge::Payload { value }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");

        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| caller["path"] == "src/consumer.rs")),
            "{context}"
        );
    }
}

#[test]
fn impact_rust_super_path_keeps_a_shared_source_caller() {
    for (module_tail, extra_path, extra_source) in [
        (
            "#[path = \"../other/child.rs\"] pub mod child;\n",
            None,
            None,
        ),
        (
            "pub mod inc;\n",
            Some("src/alias/inc.RS"),
            Some("pub use super::Payload;\n#[path = \"../other/child.rs\"] pub mod child;\n"),
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src/other");
        repo.create_dir_all("src/alias");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod other;\npub mod alias;\n");
        repo.write(
            "src/other.rs",
            "pub struct Payload { pub value: u8 }\npub mod child;\n",
        );
        repo.write(
            "src/other/child.rs",
            "pub fn caller(value: super::Payload) -> super::Payload { value }\n",
        );
        if let (Some(path), Some(source)) = (extra_path, extra_source) {
            repo.write(path, source);
        }
        repo.write(
            "src/alias/mod.rs",
            format!("pub struct Payload {{ pub value: u8 }}\n{module_tail}"),
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/alias/mod.rs",
            format!("pub struct Payload {{ pub value: u64 }}\n{module_tail}"),
        );

        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| caller["path"] == "src/other/child.rs")),
            "{extra_path:?}: {context}"
        );
    }
}

#[test]
fn impact_rust_free_function_paths_keep_only_their_module_owner() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/other");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin;\npub mod other;\npub mod real;\n",
    );
    repo.write("src/origin.rs", "pub fn create() -> u8 { 0 }\n");
    repo.write(
        "src/other.rs",
        "use std::fmt::Debug;\npub fn create() -> u8 { 1 }\npub mod child;\npub fn bare() -> u8 { create() }\npub fn self_path() -> u8 { self::create() }\npub fn absolute() -> u8 { crate::other::create() }\n",
    );
    repo.write(
        "src/other/child.rs",
        "pub fn parent() -> u8 { super::create() }\n",
    );
    repo.write(
        "src/real.rs",
        "pub fn caller() -> u8 { crate::origin::create(2) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/origin.rs",
        "pub fn create(value: u8) -> u8 { value }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == "src/real.rs")),
        "{context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket]
                .as_array()
                .is_none_or(|callers| callers.iter().all(|caller| {
                    caller["path"] != "src/other.rs" && caller["path"] != "src/other/child.rs"
                })),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_path_owner_staged_uses_index_instead_of_worktree_modules() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin;\npub mod other;\npub mod consumer;\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write("src/other.rs", "pub use crate::origin::Payload;\n");
    repo.write(
        "src/consumer.rs",
        "pub fn caller(value: crate::other::Payload) -> crate::other::Payload { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");
    repo.git(["add", "src/origin.rs"]);
    repo.write("src/other.rs", "pub struct Payload { pub value: u8 }\n");

    let context = repo.run_json("context", &["--git", "--staged"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/consumer.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_shared_source_across_cargo_targets_keeps_caller() {
    let repo = TestRepo::new();
    repo.create_dir_all("a/src/other");
    repo.create_dir_all("b");
    repo.write("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"../a/src/other.rs\"\n",
    );
    repo.write("a/src/lib.rs", "pub mod other;\npub mod sub;\n");
    repo.write("a/src/sub.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write(
        "a/src/other.rs",
        "pub mod sub;\npub fn caller(value: self::sub::Payload) -> self::sub::Payload { value }\n",
    );
    repo.write(
        "a/src/other/sub.rs",
        "pub struct Payload { pub value: u8 }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("a/src/sub.rs", "pub struct Payload { pub value: u64 }\n");

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "a/src/other.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_external_workspace_member_keeps_shared_source_caller() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/other");
    let sibling = tempfile::Builder::new()
        .prefix("shared-member-")
        .tempdir_in(repo.root().parent().unwrap())
        .unwrap();
    let sibling_name = sibling.path().file_name().unwrap().to_str().unwrap();
    let repo_name = repo.root().file_name().unwrap().to_str().unwrap();
    repo.write(
        "Cargo.toml",
        format!("[workspace]\nmembers = [\".\", \"../{sibling_name}\"]\n[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    );
    std::fs::write(
        sibling.path().join("Cargo.toml"),
        format!("[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\nworkspace = \"../{repo_name}\"\n[lib]\npath = \"../{repo_name}/src/other.rs\"\n"),
    )
    .unwrap();
    repo.write("src/lib.rs", "pub mod other;\npub mod sub;\n");
    repo.write("src/sub.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write(
        "src/other.rs",
        "pub mod sub;\npub fn caller(value: self::sub::Payload) -> self::sub::Payload { value }\n",
    );
    repo.write("src/other/sub.rs", "pub struct Payload { pub value: u8 }\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/sub.rs", "pub struct Payload { pub value: u64 }\n");

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/other.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_module_wiring_change_keeps_old_side_caller() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin;\npub mod consumer;\npub use origin as other;\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write("src/other.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write(
        "src/consumer.rs",
        "pub fn caller(value: crate::other::Payload) -> crate::other::Payload { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        "pub mod origin;\npub mod consumer;\npub mod other;\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/origin.rs")
        .unwrap();
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/consumer.rs")),
        "{context}"
    );
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{context}");
}

#[test]
fn impact_rust_private_item_reaches_child_but_not_sibling() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/foo");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\nmod other;\nmod consumer;\n");
    repo.write("src/foo.rs", "mod child;\ntype Local = u32;\n");
    repo.write(
        "src/foo/child.rs",
        "pub fn make(value: super::Local) -> super::Local { value }\n",
    );
    repo.write(
        "src/other.rs",
        "#[derive(Debug)] pub struct Marker;\npub type Local = u16;\n",
    );
    repo.write(
        "src/consumer.rs",
        "pub fn other(value: crate::other::Local) -> crate::other::Local { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/foo.rs", "mod child;\ntype Local = u64;\n");

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/foo.rs")
        .unwrap();
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/foo/child.rs")),
        "{context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(|callers| callers
                .iter()
                .all(|caller| caller["path"] != "src/consumer.rs")),
            "{bucket}: {context}"
        );
    }
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{context}");
}

#[test]
fn impact_rust_private_root_function_reaches_child_file() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "mod consumer;\nfn helper(value: u8) -> u8 { value }\n",
    );
    repo.write(
        "src/consumer.rs",
        "pub fn use_helper() -> u8 { crate::helper(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        "mod consumer;\nfn helper(value: u64) -> u64 { value }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/consumer.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_private_impl_method_reaches_descendants_only() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/foo/child");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\nmod bar;\n");
    repo.write(
        "src/foo.rs",
        "mod child;\npub struct Foo;\npub fn make_foo() -> Foo { Foo }\nimpl Foo { fn helper(&self, value: u8) -> u8 { value } }\n",
    );
    repo.write(
        "src/foo/child.rs",
        "mod grand;\nuse super::Foo;\nuse crate::bar::helper;\npub fn via_dot(f: &Foo) -> u8 { f.helper(1) }\npub fn via_ufcs(f: &Foo) -> u8 { super::Foo::helper(f, 2) }\npub fn via_free() { helper() }\npub struct Data { pub helper: u8 }\npub fn via_field(d: &Data) -> u8 { d.helper }\n",
    );
    repo.write(
        "src/foo/child/grand.rs",
        "pub fn inferred() -> u8 { let f = crate::foo::make_foo(); f.helper(3) }\n",
    );
    repo.write(
        "src/bar.rs",
        "use crate::foo::Foo;\npub struct Bar;\nimpl Bar { pub fn helper(&self, value: u8) -> u8 { value } }\npub fn helper() {}\npub fn unrelated(b: &Bar) -> u8 { b.helper(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/foo.rs",
        "mod child;\npub struct Foo;\npub fn make_foo() -> Foo { Foo }\nimpl Foo { fn helper(&self, value: u16) -> u16 { value } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/foo.rs")
        .unwrap();
    let callers = change["impacted_callers"]
        .as_array()
        .unwrap_or_else(|| panic!("{context}"));
    let child_lines: Vec<_> = callers
        .iter()
        .filter(|caller| caller["path"] == "src/foo/child.rs")
        .collect();
    assert_eq!(child_lines.len(), 2, "{context}");
    assert!(
        change["low_confidence_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/foo/child/grand.rs")),
        "{context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket]
                .as_array()
                .is_none_or(|callers| callers.iter().all(|caller| {
                    caller["path"] != "src/bar.rs"
                        && !(caller["path"] == "src/foo/child.rs"
                            && (caller["line"] == 5 || caller["line"] == 7))
                })),
            "{bucket}: {context}"
        );
    }
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{context}");
    repo.stage_all();
    let staged = repo.run_json("context", &["--git", "--staged"]);
    let staged_change = &staged["changes"][0];
    assert!(
        staged_change["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{staged}"
    );
    assert!(
        staged_change["low_confidence_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/foo/child.rs")),
        "{staged}"
    );
}

#[test]
fn impact_rust_private_impl_method_ambiguous_owner_does_not_block_unrelated_module() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/foo");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\nmod bar;\n");
    repo.write("src/foo.rs", "mod child;\npub struct Foo;\nstruct Other;\nimpl Foo { fn helper(&self, value: u8) -> u8 { value } }\nimpl Other { fn helper(&self, value: u8) -> u8 { value } }\n");
    repo.write(
        "src/foo/child.rs",
        "use super::Foo;\npub fn call(f: &Foo) -> u8 { f.helper(1) }\n",
    );
    repo.write("src/bar.rs", "use crate::foo::Foo;\npub struct Bar;\nimpl Bar { fn helper(&self, value: u8) -> u8 { value } }\npub fn unrelated(b: &Bar) -> u8 { let _ = std::mem::size_of::<Foo>(); b.helper(1) }\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/foo.rs", "mod child;\npub struct Foo;\nstruct Other;\nimpl Foo { fn helper(&self, value: u16) -> u16 { value } }\nimpl Other { fn helper(&self, value: u8) -> u8 { value } }\n");

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket]
                .as_array()
                .is_none_or(|callers| callers.iter().all(|caller| caller["path"] != "src/bar.rs")),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_private_impl_method_local_competitor_is_low_confidence() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/foo");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\n");
    repo.write(
        "src/foo.rs",
        "mod child;\npub struct Foo;\nimpl Foo { fn helper(&self, value: u8) -> u8 { value } }\n",
    );
    repo.write("src/foo/child.rs", "use super::Foo;\nstruct Local;\nimpl Local { fn helper(&self, value: u8) -> u8 { value } }\npub fn other(l: &Local) -> u8 { l.helper(1) }\npub fn true_call(f: &Foo) -> u8 { f.helper(1) }\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/foo.rs",
        "mod child;\npub struct Foo;\nimpl Foo { fn helper(&self, value: u16) -> u16 { value } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{context}"
    );
    let low = change["low_confidence_callers"]
        .as_array()
        .expect("low callers");
    assert!(
        low.iter()
            .any(|caller| caller["path"] == "src/foo/child.rs" && caller["line"] == 3),
        "{context}"
    );
    assert!(
        low.iter()
            .any(|caller| caller["path"] == "src/foo/child.rs" && caller["line"] == 4),
        "{context}"
    );
}

#[test]
fn impact_rust_private_generic_impl_method_reaches_child() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/foo");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\n");
    repo.write("src/foo.rs", "mod child;\npub struct Foo<T>(pub T);\nimpl<T> Foo<T> { fn helper<U>(&self, value: U) -> U { value } }\n");
    repo.write(
        "src/foo/child.rs",
        "use super::Foo;\npub fn call(f: &Foo<u8>) -> u8 { f.helper::<u8>(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/foo.rs", "mod child;\npub struct Foo<T>(pub T);\nimpl<T> Foo<T> { fn helper<U>(&self, value: U, _extra: u8) -> U { value } }\n");

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/foo/child.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_private_impl_method_leaf_ignores_sibling() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\nmod bar;\n");
    repo.write(
        "src/foo.rs",
        "pub struct Foo;\nimpl Foo { fn helper(&self, value: u8) -> u8 { value } }\n",
    );
    repo.write("src/bar.rs", "pub struct Bar;\nimpl Bar { fn helper(&self, value: u8) -> u8 { value } }\npub fn call(b: &Bar) -> u8 { b.helper(1) }\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/foo.rs",
        "pub struct Foo;\nimpl Foo { fn helper(&self, value: u16) -> u16 { value } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(Vec::is_empty),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_private_impl_method_body_only_change_does_not_block_callers() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/foo");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\n");
    repo.write(
        "src/foo.rs",
        "mod child;\npub struct Foo;\nimpl Foo { fn helper(&self) -> u8 { 1 } }\n",
    );
    repo.write(
        "src/foo/child.rs",
        "use super::Foo;\npub fn call(f: &Foo) -> u8 { f.helper() }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/foo.rs",
        "mod child;\npub struct Foo;\nimpl Foo { fn helper(&self) -> u8 { 2 } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{context}"
    );
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{context}");
}

#[test]
fn impact_rust_private_sibling_is_excluded_with_common_project_syntax() {
    for variation in [
        "expression macro",
        "module attribute",
        "two roots",
        "unused source",
        "test module",
        "body macro edit",
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write(
            "src/lib.rs",
            if variation == "module attribute" {
                "mod a;\nmod b;\nmod util;\n#[cfg(test)] mod tests;\n"
            } else {
                "mod a;\nmod b;\nmod util;\n"
            },
        );
        repo.write(
            "src/a.rs",
            if variation == "test module" {
                "fn helper(value: u8) -> u8 { value }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(helper(1), 1); } }\n"
            } else if variation == "body macro edit" {
                "fn helper(value: u8) -> u8 { println!(\"old\"); value }\n"
            } else {
                "fn helper(value: u8) -> u8 { value }\n"
            },
        );
        repo.write("src/util.rs", "pub fn helper(value: u8) -> u8 { value }\n");
        repo.write(
            "src/b.rs",
            if variation == "expression macro" {
                "use crate::util::helper;\npub fn call() -> u8 { println!(\"x\"); helper(1) }\n"
            } else {
                "use crate::util::helper;\npub fn call() -> u8 { helper(1) }\n"
            },
        );
        if variation == "two roots" {
            repo.write("src/main.rs", "fn main() {}\n");
        }
        if variation == "unused source" {
            repo.write("src/orphan.rs", "fn check() { assert!(true); }\n");
        }
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/a.rs",
            if variation == "test module" {
                "fn helper(value: u64) -> u64 { value }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(helper(1), 1); } }\n"
            } else if variation == "body macro edit" {
                "fn helper(value: u64) -> u64 { println!(\"new\"); value }\n"
            } else {
                "fn helper(value: u64) -> u64 { value }\n"
            },
        );

        let context = repo.run_json("context", &["--git"]);
        for bucket in [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers",
        ] {
            assert!(
                context["changes"][0][bucket]
                    .as_array()
                    .is_none_or(|callers| callers
                        .iter()
                        .all(|caller| caller["path"] != "src/b.rs")),
                "{variation} {bucket}: {context}"
            );
        }
        let output = cargo_bin()
            .args([
                "impact",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{variation}: {context}");
    }
}

#[test]
fn impact_rust_private_direct_import_works_inside_workspace() {
    let repo = TestRepo::new();
    repo.create_dir_all("crates/sample/src");
    repo.write("Cargo.toml", "[workspace]\nmembers = [\"crates/sample\"]\n");
    repo.write(
        "crates/sample/Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("crates/sample/src/lib.rs", "mod a;\nmod b;\nmod util;\n");
    repo.write(
        "crates/sample/src/a.rs",
        "fn helper(value: u8) -> u8 { value }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(helper(1), 1); } }\n",
    );
    repo.write(
        "crates/sample/src/util.rs",
        "pub fn helper(value: u8) -> u8 { value }\n",
    );
    repo.write(
        "crates/sample/src/b.rs",
        "use crate::util::helper;\npub fn call() -> u8 { helper(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "crates/sample/src/a.rs",
        "fn helper(value: u64) -> u64 { value }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(helper(1), 1); } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(|callers| callers
                .iter()
                .all(|caller| caller["path"] != "crates/sample/src/b.rs")),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_private_leaf_ignores_unrelated_sibling_function() {
    for (variation, extra) in [
        (
            "test module",
            "#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(helper(1), 1); } }\n",
        ),
        (
            "derive and formatting",
            "#[derive(serde::Serialize)] struct Record;\nimpl std::fmt::Display for Record { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, \"record\") } }\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n",
        );
        repo.write("src/lib.rs", "mod a;\nmod b;\n");
        repo.write(
            "src/a.rs",
            format!("fn helper(value: u8) -> u8 {{ value }}\n{extra}"),
        );
        repo.write(
            "src/b.rs",
            "fn helper() -> u8 { println!(\"x\"); 1 }\npub fn call() -> u8 { helper() }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/a.rs",
            format!("fn helper(value: u64) -> u64 {{ value }}\n{extra}"),
        );

        let context = repo.run_json("context", &["--git"]);
        for bucket in [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers",
        ] {
            assert!(
                context["changes"][0][bucket]
                    .as_array()
                    .is_none_or(|callers| callers
                        .iter()
                        .all(|caller| caller["path"] != "src/b.rs")),
                "{variation} {bucket}: {context}"
            );
        }
    }
}

#[test]
fn impact_rust_visibility_widening_keeps_existing_child_without_sibling_noise() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/model");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod model; mod other; mod state;\n");
    repo.write(
        "src/model.rs",
        "struct Range { start: usize }\nmod child;\n",
    );
    repo.write(
        "src/model/child.rs",
        "pub fn make() -> super::Range { super::Range { start: 1 } }\n",
    );
    repo.write("src/state.rs", "pub struct Range(pub u8);\n");
    repo.write(
        "src/other.rs",
        "use crate::state::Range;\npub fn take(value: Range) -> Range { println!(\"ok\"); value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    for visibility in ["pub(crate)", "pub"] {
        repo.write(
            "src/model.rs",
            format!("{visibility} struct Range {{ start: usize }}\nmod child;\n"),
        );
        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/model.rs")
            .unwrap();
        assert!(
            change["informational_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| caller["path"] == "src/model/child.rs")),
            "{visibility}: {context}"
        );
        for bucket in [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers",
        ] {
            assert!(
                change[bucket].as_array().is_none_or(|callers| callers
                    .iter()
                    .all(|caller| caller["path"] != "src/other.rs")),
                "{visibility} {bucket}: {context}"
            );
        }
        for command in ["impact", "review"] {
            let output = cargo_bin()
                .args([
                    command,
                    "--dir",
                    repo.root().to_str().unwrap(),
                    "--git",
                    "--hook",
                ])
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "{visibility} {command}: {context} {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
}

#[test]
fn impact_rust_visibility_widening_function_is_informational() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/model");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod model; mod other; mod state;\n");
    repo.write(
        "src/model.rs",
        "fn helper(value: u8) -> u8 { value }\nmod child;\n",
    );
    repo.write(
        "src/model/child.rs",
        "pub fn call() -> u8 { super::helper(1) }\n",
    );
    repo.write("src/state.rs", "pub fn helper(value: u8) -> u8 { value }\n");
    repo.write(
        "src/other.rs",
        "use crate::state::helper;\npub fn call() -> u8 { helper(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/model.rs",
        "pub(crate) fn helper(value: u8) -> u8 { value }\nmod child;\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert!(
        change["informational_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/model/child.rs")),
        "{context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(|callers| callers
                .iter()
                .all(|caller| caller["path"] != "src/other.rs")),
            "{bucket}: {context}"
        );
    }
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{context}");
    let review = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(
        review.status.code(),
        Some(0),
        "{context} {}",
        String::from_utf8_lossy(&review.stdout)
    );

    repo.write(
        "src/model.rs",
        "pub(crate) fn helper(value: u64) -> u64 { value }\nmod child;\n",
    );
    let changed_contract = repo.run_json("context", &["--git"]);
    assert!(
        changed_contract["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/model/child.rs")),
        "{changed_contract}"
    );
}

#[test]
fn impact_rust_visibility_widening_keeps_glob_ambiguity_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod model; mod other;\n");
    repo.write("src/model.rs", "struct Range<T>(T);\n");
    repo.write(
        "src/other.rs",
        "use crate::model::*;\nuse std::ops::*;\npub fn take(value: Range<usize>) -> Range<usize> { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/model.rs", "pub(crate) struct Range<T>(T);\n");

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/other.rs")),
        "{context}"
    );
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{context}");
}

#[test]
fn impact_rust_private_direct_import_does_not_hide_qualified_child_call() {
    for (label, imports, body) in [
        ("qualified", "use crate::util::helper;", "super::helper(1)"),
        (
            "block import",
            "use crate::util::helper;",
            "{ use super::*; helper(1) }",
        ),
        (
            "conditional root import",
            "#[cfg(not(test))] use crate::util::helper;\n#[cfg(test)] use super::helper;",
            "helper(1)",
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write(
            "src/lib.rs",
            "mod a;\nmod b;\nmod util;\nfn helper(value: u8) -> u8 { value }\n",
        );
        repo.write(
            "src/a.rs",
            "fn helper(value: u8) -> u8 { value }\n#[path = \"b.rs\"] mod b2;\n",
        );
        repo.write(
            "src/b.rs",
            format!("{imports}\npub fn call() -> u8 {{ {body} }}\n"),
        );
        repo.write("src/util.rs", "pub fn helper(value: u8) -> u8 { value }\n");
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/a.rs",
            "fn helper(value: u64) -> u64 { value }\n#[path = \"b.rs\"] mod b2;\n",
        );

        let context = repo.run_json("context", &["--git"]);
        let call_line = if label == "conditional root import" {
            2
        } else {
            1
        };
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| { caller["path"] == "src/b.rs" && caller["line"] == call_line })),
            "{label}: {context}"
        );
    }
}

#[test]
fn impact_rust_private_name_uses_sibling_local_declaration_with_test_module() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod model;\nmod consumer;\n");
    repo.write("src/model.rs", "type Payload = u8;\n");
    repo.write(
        "src/consumer.rs",
        "type Payload = u16;\npub fn use_it(value: Payload) -> Payload { value }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() {} }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/model.rs", "type Payload = u32;\n");

    let context = repo.run_json("context", &["--git"]);
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            context["changes"][0][bucket]
                .as_array()
                .is_none_or(|callers| callers
                    .iter()
                    .all(|caller| caller["path"] != "src/consumer.rs")),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_private_struct_with_impl_keeps_child_and_ignores_sibling() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/model");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod model;\nmod consumer;\n");
    repo.write(
        "src/model.rs",
        "mod child;\nstruct Foo { value: u8 }\nimpl Foo { fn make(value: u8) -> Self { Self { value } } }\n",
    );
    repo.write(
        "src/model/child.rs",
        "pub fn make() -> super::Foo { super::Foo::make(1) }\n",
    );
    repo.write(
        "src/consumer.rs",
        "struct Foo { value: u16 }\npub fn make() -> Foo { Foo { value: 1 } }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() {} }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/model.rs",
        "mod child;\nstruct Foo { value: u64 }\nimpl Foo { fn make(value: u8) -> Self { Self { value: value.into() } } }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = &context["changes"][0];
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/model/child.rs")),
        "{context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(|callers| callers
                .iter()
                .all(|caller| caller["path"] != "src/consumer.rs")),
            "{bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_visibility_narrowing_keeps_former_caller() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod foo;\nmod consumer;\n");
    repo.write("src/foo.rs", "pub(crate) type Local = u32;\n");
    repo.write(
        "src/consumer.rs",
        "pub fn make(value: crate::foo::Local) -> crate::foo::Local { value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/foo.rs", "type Local = u32;\n");

    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/consumer.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_absolute_grouped_use_shadows_standard_type() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.create_dir_all("shapes/src");
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"shapes\"]\n[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshapes = { path = \"shapes\" }\n",
    );
    repo.write(
        "shapes/Cargo.toml",
        "[package]\nname = \"shapes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("shapes/src/lib.rs", "pub struct Range { pub end: usize }\n");
    repo.write(
        "src/lib.rs",
        "use std::ops::Range;\npub fn standard(r: Range<usize>) -> usize { r.end }\npub fn plain() -> usize { use shapes::Range; let r = Range { end: 1 }; r.end }\npub fn grouped() -> usize { use ::{shapes::Range}; let r = Range { end: 1 }; r.end }\npub fn aliased() -> usize { use ::{shapes::Range as Range}; let r = Range { end: 1 }; r.end }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("shapes/src/lib.rs", "pub struct Range { pub end: u64 }\n");

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "shapes/src/lib.rs")
        .unwrap();
    let callers = change["impacted_callers"].as_array().unwrap();
    for name in ["plain", "grouped", "aliased"] {
        assert!(
            callers.iter().any(|caller| caller["name"] == name),
            "missing {name}: {context}"
        );
    }
    assert!(
        callers.iter().all(|caller| caller["name"] != "standard"),
        "{context}"
    );
}

#[test]
fn impact_rust_unreachable_cargo_package_does_not_claim_same_named_type() {
    let repo = TestRepo::new();
    for package in ["a", "b", "c"] {
        repo.create_dir_all(format!("{package}/src"));
    }
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
    );
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"sample-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "b/Cargo.toml",
        "[package]\nname = \"sample-b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nanyhow = \"1\"\n",
    );
    repo.write(
        "c/Cargo.toml",
        "[package]\nname = \"sample-c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nsample-a = { path = \"../a\" }\n",
    );
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, ()>;\n",
    );
    repo.write(
        "b/src/lib.rs",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { Ok(()) }\n",
    );
    repo.write(
        "c/src/lib.rs",
        "use sample_a::Result;\npub fn caller() -> Result<()> { Ok(()) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, u8>;\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "a/src/lib.rs")
        .unwrap();
    let callers = change["impacted_callers"].as_array().unwrap();
    assert!(
        callers
            .iter()
            .any(|caller| caller["path"] == "c/src/lib.rs"),
        "{context}"
    );
    assert!(
        callers
            .iter()
            .all(|caller| caller["path"] != "b/src/lib.rs"),
        "{context}"
    );
    assert!(
        change["informational_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .all(|caller| caller["path"] != "b/src/lib.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_source_included_by_another_package_keeps_real_caller() {
    for layout in ["direct", "nested", "upper_ext", "unicode_ext"] {
        let repo = TestRepo::new();
        for package in ["a", "b", "d"] {
            repo.create_dir_all(format!("{package}/src"));
        }
        repo.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"d\"]\n",
        );
        for package in ["a", "b", "d"] {
            repo.write(
            format!("{package}/Cargo.toml"),
            format!(
                "[package]\nname = \"sample-{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{}",
                if package == "d" {
                    "[dependencies]\nsample-a = { path = \"../a\" }\n"
                } else {
                    ""
                }
            ),
        );
        }
        repo.write(
            "a/src/lib.rs",
            "pub type Result<T> = std::result::Result<T, ()>;\n",
        );
        repo.write("b/src/lib.rs", "pub fn unrelated() {}\n");
        repo.write(
            "b/src/shared.rs",
            "use sample_a::Result;\npub fn caller() -> Result<()> { Ok(()) }\n",
        );
        if layout == "nested" {
            repo.create_dir_all("d/src/vendor");
            repo.write("d/src/lib.rs", "mod vendor;\n");
            repo.write(
                "d/src/vendor/mod.rs",
                "#[path = \"../../../b/src/shared.rs\"] mod shared;\n",
            );
        } else if matches!(layout, "upper_ext" | "unicode_ext") {
            repo.write("d/src/lib.rs", "mod inc;\n");
            repo.write(
                if layout == "upper_ext" {
                    "d/src/inc.RS"
                } else {
                    "d/src/inc.rſ"
                },
                "#[path = \"../../b/src/shared.rs\"] mod shared;\n",
            );
        } else {
            repo.write(
                "d/src/lib.rs",
                "#[path = \"../../b/src/shared.rs\"] mod shared;\n",
            );
        }
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "a/src/lib.rs",
            "pub type Result<T> = std::result::Result<T, u8>;\n",
        );

        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "a/src/lib.rs")
            .unwrap();
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| caller["path"] == "b/src/shared.rs")),
            "layout={layout}: {context}"
        );
    }
}

#[test]
fn impact_rust_cargo_graph_keeps_callers_when_manifest_changes() {
    let repo = TestRepo::new();
    repo.create_dir_all("a/src");
    repo.create_dir_all("b/src");
    repo.write("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"sample-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "b/Cargo.toml",
        "[package]\nname = \"sample-b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nsample-a = { path = \"../a\" }\n",
    );
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, ()>;\n",
    );
    repo.write(
        "b/src/lib.rs",
        "use sample_a::Result;\npub fn caller() -> Result<()> { Ok(()) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, u8>;\n",
    );
    repo.stage_all();
    repo.write(
        "b/Cargo.toml",
        "[package]\nname = \"sample-b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );

    for arguments in [&["--git", "--staged"][..], &["--git"][..]] {
        let context = repo.run_json("context", arguments);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "a/src/lib.rs")
            .unwrap();
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| caller["path"] == "b/src/lib.rs")),
            "{arguments:?}: {context}"
        );
    }
}

#[test]
fn impact_rust_cargo_config_deletion_keeps_same_named_caller() {
    let repo = TestRepo::new();
    for package in ["a", "b"] {
        repo.create_dir_all(format!("{package}/src"));
    }
    repo.create_dir_all(".cargo");
    repo.write("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    repo.write(".cargo/config.toml", "[build]\n");
    for package in ["a", "b"] {
        repo.write(
            format!("{package}/Cargo.toml"),
            format!(
                "[package]\nname = \"sample-{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
            ),
        );
    }
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, ()>;\n",
    );
    repo.write(
        "b/src/lib.rs",
        "use anyhow::Result;\npub fn unrelated() -> Result<()> { Ok(()) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "a/src/lib.rs",
        "pub type Result<T> = std::result::Result<T, u8>;\n",
    );
    repo.remove_file(".cargo/config.toml");
    std::fs::remove_dir(repo.root().join(".cargo")).unwrap();

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "a/src/lib.rs")
        .unwrap();
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "b/src/lib.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_one_line_body_change_does_not_block_callers() {
    for (replacement, should_block) in [
        ("pub fn helper() -> u8 { 2 }\n", false),
        ("pub fn helper() -> u16 { 2 }\n", true),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write("src/lib.rs", "pub mod api;\nmod consumer;\n");
        repo.write("src/api.rs", "pub fn helper() -> u8 { 1 }\n");
        repo.write(
            "src/consumer.rs",
            "use crate::api::helper;\npub fn run() -> u8 { helper() }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/api.rs", replacement);

        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/api.rs")
            .unwrap();
        let blocking = change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| {
                callers
                    .iter()
                    .any(|caller| caller["path"] == "src/consumer.rs")
            });
        assert_eq!(blocking, should_block, "{replacement}: {context}");

        let output = cargo_bin()
            .args([
                "impact",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(if should_block { 1 } else { 0 }),
            "{replacement}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn impact_suppresses_only_proven_rust_type_alias_equivalence() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        "pub mod consumer;\n\npub fn collect() -> Vec<(u64, String)> { vec![] }\npub fn changed() -> Vec<(u64, String)> { vec![] }\npub fn local() { let _ = collect(); }\n",
    );
    repo.write(
        "src/consumer.rs",
        "use crate::{changed, collect};\n\npub fn run() {\n    let _ = collect();\n    let _ = changed();\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");

    repo.write(
        "src/lib.rs",
        "pub mod consumer;\n\npub type Entry = (u64, String);\npub type ChangedEntry = (u32, String);\n\npub fn collect() -> Vec<Entry> { vec![] }\npub fn changed() -> Vec<ChangedEntry> { vec![] }\npub fn local() { let _ = collect(); }\n",
    );
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run alias impact");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "異なる alias RHS の対照ケースは unresolved impact を維持すべき"
    );
    let source_header = stderr
        .lines()
        .find(|line| line.starts_with("src/lib.rs changed ["))
        .expect("changed source header should be present");
    assert_eq!(
        source_header, "src/lib.rs changed [changed]:",
        "同値 alias の collect を source header に混ぜるべきでない: {stderr}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("src/consumer.rs:5 [changed]")),
        "異なる型契約の caller は報告すべき: {stderr}"
    );

    let diff_output = Command::new("git")
        .args(["diff", "--no-ext-diff"])
        .current_dir(repo.root())
        .output()
        .expect("git diff should run");
    assert!(diff_output.status.success());
    let diff = String::from_utf8(diff_output.stdout).expect("git diff should be utf-8");
    let result = astro_sight::service::AppService::new()
        .analyze_context(
            &diff,
            repo.root().to_str().expect("utf-8 path"),
            &astro_sight::models::impact::ContextAnalysisOptions::default(),
        )
        .expect("context analysis should succeed");
    let change = result
        .changes
        .iter()
        .find(|change| change.path == "src/lib.rs")
        .expect("lib.rs impact should remain visible");
    assert!(
        change
            .signature_changes
            .iter()
            .any(|signature| signature.name == "collect"),
        "証明済み alias 導入も signature change の痕跡を残すべき"
    );
    assert!(
        change.informational_callers.iter().any(|caller| {
            caller.path.ends_with("src/consumer.rs")
                && caller.symbols == vec!["collect".to_string()]
        }),
        "証明済み alias 導入の caller は informational に残すべき: {:?}",
        change.informational_callers
    );
    assert!(
        change.informational_callers.iter().any(|caller| {
            caller.path == "src/lib.rs" && caller.symbols == vec!["collect".to_string()]
        }),
        "同一ファイル内 caller も消さず informational に残すべき: {:?}",
        change.informational_callers
    );
    assert!(
        change
            .impacted_callers
            .iter()
            .all(|caller| !caller.symbols.contains(&"collect".to_string())),
        "証明済み alias 導入は blocking caller に残すべきでない: {:?}",
        change.impacted_callers
    );
}

#[test]
fn impact_rust_alias_equivalence_fails_closed_for_rebound_prelude_name() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        "pub mod consumer;\npub struct SA; pub struct SB;\nuse crate::SA as String;\npub fn collect() -> (u64, String) { unimplemented!() }\n",
    );
    repo.write(
        "src/consumer.rs",
        "use crate::collect;\npub fn run() { let _ = collect(); }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        "pub mod consumer;\npub struct SA; pub struct SB;\nuse crate::SB as String;\npub type Entry = (u64, String);\npub fn collect() -> Entry { unimplemented!() }\n",
    );

    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run rebound-name impact");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "String という字面が同じでも import が別型へ変われば blocking を維持すべき"
    );
    assert!(
        stderr.contains("src/consumer.rs:2 [collect]"),
        "破壊的な import 再束縛の caller を報告すべき: {stderr}"
    );
}

#[test]
fn impact_rust_alias_equivalence_requires_root_level_function() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        "pub mod consumer;\npub struct ExtA; pub struct ExtB;\nuse crate::ExtA as Ext;\npub type Entry = (Ext, String);\npub mod inner { use crate::ExtB as Ext; pub fn collect() -> Vec<(Ext, String)> { vec![] } }\n",
    );
    repo.write(
        "src/consumer.rs",
        "pub fn run() { let _ = crate::inner::collect(); }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        "pub mod consumer;\npub struct ExtA; pub struct ExtB;\nuse crate::ExtA as Ext;\npub type Entry = (Ext, String);\npub mod inner { use crate::ExtB as Ext; use super::Entry; pub fn collect() -> Vec<Entry> { vec![] } }\n",
    );

    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run nested-scope impact");
    assert!(
        !output.status.success(),
        "nested function へ root alias の証明を流用せず blocking を維持すべき: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn impact_rust_alias_const_expression_keeps_callers_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        "pub const fn size() -> usize { 4 }\npub fn collect() -> [u8; size()] { [0; size()] }\n",
    );
    repo.write(
        "src/consumer.rs",
        "pub fn run() { let _ = crate::collect(); }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        "pub const fn size() -> usize { 8 }\npub type Entry = [u8; size()];\npub fn collect() -> Entry { [0; size()] }\n",
    );

    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run const-expression impact");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "caller must stay blocking");
    assert!(
        stderr.contains("src/consumer.rs:1 [collect]"),
        "value-dependent alias must not suppress the caller: {stderr}"
    );
}

#[test]
fn impact_distinguishes_rust_binding_mut_from_type_mutability() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        "pub mod consumer;\n\npub fn apply<F: FnMut(u32)>(mut callback: F) {\n    drop(callback);\n}\n\npub fn borrow(value: &mut u32) -> u32 {\n    *value\n}\n",
    );
    repo.write(
        "src/consumer.rs",
        "use crate::{apply, borrow};\n\npub fn run(value: &mut u32) {\n    apply(|_| {});\n    let _ = borrow(value);\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");

    repo.write(
        "src/lib.rs",
        "pub mod consumer;\n\npub fn apply<F: FnMut(u32)>(callback: F) {\n    drop(callback);\n}\n\npub fn borrow(value: &mut u32) -> u32 {\n    *value\n}\n",
    );
    let binding_only = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run binding-only impact");
    assert!(
        binding_only.status.success(),
        "binding-side の mut 除去だけなら unresolved impact にすべきでない: {}",
        String::from_utf8_lossy(&binding_only.stderr)
    );

    repo.commit_all("remove binding mut");
    repo.write(
        "src/lib.rs",
        "pub mod consumer;\n\npub fn apply<F: FnMut(u32)>(callback: F) {\n    drop(callback);\n}\n\npub fn borrow(value: &u32) -> u32 {\n    *value\n}\n",
    );
    let type_change = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run type-change impact");
    let stderr = String::from_utf8_lossy(&type_change.stderr);
    assert!(
        !type_change.status.success(),
        "型側の &mut T -> &T は unresolved impact に残すべき"
    );
    assert!(
        stderr.contains("consumer.rs") && stderr.contains("borrow"),
        "型契約変更の caller とシンボルを報告すべき: {stderr}"
    );
}

#[test]
fn impact_rust_local_var_not_treated_as_cross_file_ref() {
    // Issue 2026-06-13-ai-status-json-symbol-fp: 別ファイルのローカル変数 `let json` が
    // 同名の自由関数 `render::json` への cross-file 参照に誤マッチしないこと。
    // qualified call (`render::json`) を持つ main.rs は high のまま、`let json` だけの
    // profiles.rs は未解決影響に出ないことを検証する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    // profiles.rs: render::json を import / qualified 参照せず bare `let json` のみ
    std::fs::write(
        root.join("src/profiles.rs"),
        "use std::fs;\npub fn discover() -> i32 {\n    let json = fs::read_to_string(\"x\").unwrap_or_default();\n    json.trim().parse().unwrap_or(0)\n}\n",
    )
    .unwrap();
    // main.rs: render::json を qualified path で呼ぶ実 caller
    std::fs::write(
        root.join("src/main.rs"),
        "mod render;\nmod profiles;\nfn main() {\n    println!(\"{}\", render::json(profiles::discover()));\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // render::json のシグネチャを変更 (i32 -> i64)
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // main.rs (qualified call) は未解決影響として残る
    assert!(
        stderr.contains("main.rs"),
        "qualified call (render::json) を持つ main.rs は high のまま残るべき: {stderr}"
    );
    // profiles.rs (local `let json`) は cross-file 参照ではないので出ない
    assert!(
        !stderr.contains("profiles.rs"),
        "local 変数 `let json` だけの profiles.rs は未解決影響に出るべきでない: {stderr}"
    );
}

#[test]
fn impact_kotlin_nested_function_is_not_cross_file_origin() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Local.kt"),
        "fun outer() {\n    fun parseLocal(value: Int): Int = value\n    parseLocal(1)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Consumer.kt"),
        "fun unrelated() {\n    parseLocal(\"unrelated\")\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git の起動に失敗");
        assert!(status.success(), "git {args:?} が失敗");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("Local.kt"),
        "fun outer() {\n    fun parseLocal(value: Long): Long = value\n    parseLocal(1)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("impact の起動に失敗");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "ローカル関数の変更だけなら未解決影響を報告すべきでない: {stderr}"
    );
    assert!(
        !stderr.contains("Consumer.kt"),
        "別ファイルの同名呼び出しをローカル関数の利用箇所にすべきでない: {stderr}"
    );
}

#[test]
fn impact_rust_macro_arg_ident_stays_high() {
    // Issue 2026-06-13 codex 指摘: `call_render!(json, ..)` のように macro が `crate::render::json`
    // を補うケースでは、caller に `::json` 証拠がなくても本物の参照なので high 維持すべき
    // (証拠なし bare identifier の low routing が macro 引数を取りこぼさないこと)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    // macro が path を補い、caller は bare ident `json` を渡すだけ。
    // codex 指摘の混在行 (`let json = call_render!(json, 1); json`) も同行に local binding と
    // macro 引数が混ざるケースとして含め、macro 引数側を取りこぼさないことを検証する。
    std::fs::write(
        root.join("src/caller.rs"),
        "#[macro_export]\nmacro_rules! call_render {\n    ($name:ident, $arg:expr) => { $crate::render::$name($arg) };\n}\npub fn run() -> String { let json = call_render!(json, 1); json }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "#[macro_use]\nmod caller;\nmod render;\nfn main() { let _ = caller::run(); }\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // caller.rs の macro 引数 `json` は high のまま未解決影響に残る (fail-closed)。
    assert!(
        stderr.contains("caller.rs"),
        "macro 引数の bare ident `json` は high 維持されるべき (fail-closed): {stderr}"
    );
}

#[test]
fn impact_rust_macro_callee_ident_routes_low() {
    // Issue 2026-06-27-render-json-vs-serde-json-macro:
    // `serde_json::json!` の callee 名は同名の Rust function とは別名前空間なので、
    // `render::json` のシグネチャ変更の未解決 impact にしない。一方で macro 引数は
    // `impact_rust_macro_arg_ident_stays_high` で high 維持を別途担保する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/caller.rs"),
        "use serde_json::json;\npub fn run() {\n    let _payload = json!({ \"ok\": true });\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "mod caller;\nmod render;\nfn main() { caller::run(); }\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "macro callee だけなら unresolved impact は出ないべき: {stderr}"
    );
    assert!(
        !stderr.contains("caller.rs"),
        "serde_json::json! の callee 名は render::json の impact caller ではない: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// impact command tests
// ---------------------------------------------------------------------------

#[test]
fn impact_clean_pass() {
    use std::io::Write;
    use std::process::Stdio;

    // Empty diff → exit 0, no output
    let mut child = cargo_bin()
        .args(["impact", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"")
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success(), "expected exit 0 for empty diff");
    assert!(output.stdout.is_empty(), "expected no stdout");
}

#[test]
fn impact_with_unresolved() {
    use std::io::Write;
    use std::process::Stdio;

    // diff: extract_symbols のシグネチャを変更 → 他ファイルの caller が未解決になる
    // 行番号は実コードから動的に取得する。
    let symbols_src = std::fs::read_to_string("src/engine/symbols/mod.rs")
        .expect("read src/engine/symbols/mod.rs");
    let extract_line_idx = symbols_src
        .lines()
        .position(|l| l.starts_with("pub fn extract_symbols("))
        .expect("extract_symbols 関数が見つからない");
    let line_no = extract_line_idx + 1;
    let diff = format!(
        "--- a/src/engine/symbols/mod.rs\n\
         +++ b/src/engine/symbols/mod.rs\n\
         @@ -{line_no},7 +{line_no},7 @@\n\
         -pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {{\n\
         +pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {{\n\
             let query_src = symbol_query(lang_id);\n"
    );

    let mut child = cargo_bin()
        .args(["impact", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    assert!(
        !output.status.success(),
        "expected exit 1 for unresolved impacts"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unresolved impacts found"),
        "expected 'Unresolved impacts found' in stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("extract_symbols"),
        "expected 'extract_symbols' in stderr, got: {stderr}"
    );
}

#[test]
fn impact_git_mode() {
    // --git with HEAD base on a clean repo → exit 0 (no diff = no unresolved)
    let output = cargo_bin()
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "expected exit 0 for clean git diff"
    );
    assert!(output.stdout.is_empty(), "expected no stdout");
}

#[test]
fn impact_excludes_target_test_refs() {
    use std::io::Write;
    use std::process::Stdio;

    // Create fixture: lib.rs with pub fn, consumer.rs with prod + test usage
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    std::fs::write(
        &lib_rs,
        r#"pub fn do_work(x: i32) -> i32 {
    x + 1
}
"#,
    )
    .unwrap();

    std::fs::write(
        &consumer_rs,
        r#"use crate::lib::do_work;

pub fn run() -> i32 {
    do_work(42)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run() {
        assert_eq!(do_work(1), 2);
    }
}
"#,
    )
    .unwrap();

    // Diff that changes do_work signature
    let diff = r#"--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
-pub fn do_work(x: i32) -> i32 {
+pub fn do_work(x: i32, y: i32) -> i32 {
     x + 1
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should detect unresolved impact in consumer.rs production code (line 4: do_work(42))
    assert!(
        !output.status.success(),
        "expected exit 1 for unresolved impacts"
    );
    assert!(
        stderr.contains("consumer.rs"),
        "expected consumer.rs in stderr: {stderr}"
    );

    // Verify test-context refs are excluded:
    // Only production-code caller (line 4) should appear, NOT the #[cfg(test)] ref (line 14)
    let lines: Vec<&str> = stderr.lines().collect();
    let consumer_lines: Vec<&str> = lines
        .iter()
        .filter(|l| l.contains("consumer.rs:"))
        .copied()
        .collect();

    assert!(
        !consumer_lines.is_empty(),
        "expected at least one consumer.rs caller"
    );

    for line in &consumer_lines {
        // Extract line number after "consumer.rs:"
        if let Some(pos) = line.find("consumer.rs:") {
            let after = &line[pos + "consumer.rs:".len()..];
            let line_num: usize = after
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            assert!(
                line_num < 8,
                "test-context ref at line {line_num} should be excluded: {line}"
            );
        }
    }
}

#[test]
fn impact_additive_impl_block_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Fixture: types.rs with a struct, consumer.rs that uses the struct
    let dir = tempfile::tempdir().expect("tempdir");
    let types_rs = dir.path().join("types.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // types.rs: struct with an existing impl and a NEW impl block being added
    std::fs::write(
        &types_rs,
        r#"pub struct HookInput {
    pub name: String,
}

impl HookInput {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

impl HookInput {
    pub fn bash_command(&self) -> &str {
        &self.name
    }

    pub fn file_path(&self) -> &str {
        &self.name
    }
}
"#,
    )
    .unwrap();

    // consumer.rs: uses HookInput (struct construction + method call)
    std::fs::write(
        &consumer_rs,
        r#"use crate::types::HookInput;

pub fn run() -> String {
    let input = HookInput::new("test".to_string());
    input.name.clone()
}
"#,
    )
    .unwrap();

    // Diff: adding a new impl block with new methods (backward-compatible)
    let diff = r#"--- a/types.rs
+++ b/types.rs
@@ -9,3 +9,13 @@
     }
 }
+
+impl HookInput {
+    pub fn bash_command(&self) -> &str {
+        &self.name
+    }
+
+    pub fn file_path(&self) -> &str {
+        &self.name
+    }
+}
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Adding new methods to a type is backward-compatible.
    // consumer.rs should NOT be reported as impacted.
    assert!(
        output.status.success(),
        "expected exit 0 (no unresolved impacts) for additive impl block.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("consumer.rs"),
        "consumer.rs should not appear as impacted for additive impl block: {stderr}"
    );
}

/// エクスポートシンボルの body (内部実装) のみが変わったとき、
/// import/re-export 行しか参照のないファイルは impact に載せない。
/// (レポート 2026-04-08-commitstore-internal-change-false-positive.md の再現)
#[test]
fn impact_body_only_change_import_only_callers_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let commit_store = dir.path().join("commitStore.ts");
    let index_ts = dir.path().join("index.ts");
    let app_tsx = dir.path().join("App.tsx");

    std::fs::write(
        &commit_store,
        r#"export function useCommitStore() {
  return { validate: async () => true };
}
"#,
    )
    .unwrap();
    std::fs::write(
        &index_ts,
        r#"export { useCommitStore } from "./commitStore";
"#,
    )
    .unwrap();
    std::fs::write(
        &app_tsx,
        r#"import { useCommitStore } from "./commitStore";
function App() { return null; }
"#,
    )
    .unwrap();

    // useCommitStore の body のみを変更 (宣言行は不変)
    let diff = r#"--- a/commitStore.ts
+++ b/commitStore.ts
@@ -1,3 +1,7 @@
 export function useCommitStore() {
-  return { validate: async () => true };
+  let currentRequestId = 0;
+  return {
+    validate: async () => { currentRequestId += 1; return currentRequestId > 0; },
+  };
 }
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // import / re-export しかしていないファイルは impact に載せない
    assert!(
        output.status.success(),
        "expected exit 0 (no unresolved impacts) for body-only change.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("index.ts"),
        "re-export-only file should not appear in impact: {stderr}"
    );
    assert!(
        !stderr.contains("App.tsx"),
        "import-only file should not appear in impact: {stderr}"
    );
}

/// 変更ファイルに新規追加シンボルが複数あっても、影響先ファイルの行が実際に
/// 参照しているシンボルだけが impact に紐付き、他の無関係な変更シンボルが
/// 巻き添えで紐付かない（バルク紐付け禁止）。private シンボルも同様に外部ファイルへ
/// 影響伝播しない。
/// (レポート 2026-03-19-dnspacket-bulk-symbol-binding.md の再現)
#[test]
fn impact_bulk_symbol_binding_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let src_kt = dir.path().join("DnsPacket.kt");
    let test_kt = dir.path().join("DnsPacketTest.kt");

    std::fs::write(
        &src_kt,
        r#"package pkg

class DnsPacket(val data: ByteArray) {
    companion object {
        fun createZeroResponse(packet: DnsPacket): ByteArray = byteArrayOf()
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        &test_kt,
        r#"package pkg

class DnsPacketTest {
    fun zero_response() {
        val packet = DnsPacket(byteArrayOf())
        DnsPacket.createZeroResponse(packet)
    }
}
"#,
    )
    .unwrap();

    // createServFailResponse と private createIpResponse を新規追加
    let diff = r#"--- a/DnsPacket.kt
+++ b/DnsPacket.kt
@@ -4,5 +4,7 @@
 class DnsPacket(val data: ByteArray) {
     companion object {
         fun createZeroResponse(packet: DnsPacket): ByteArray = byteArrayOf()
+        fun createServFailResponse(packet: DnsPacket): ByteArray = byteArrayOf(1)
+        private fun createIpResponse(packet: DnsPacket, ip: String): ByteArray = byteArrayOf()
     }
 }
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // DnsPacketTest.kt は createZeroResponse しか参照していない & 新規追加は未使用
    // → 無関係な変更シンボル (createServFailResponse / createIpResponse) が巻き添えで
    //   紐付かないこと、および impact として unresolved 扱いにならないことを確認する
    assert!(
        !stderr.contains("createServFailResponse"),
        "未使用の新規関数は他ファイルの impact に紐付けてはならない: {stderr}"
    );
    assert!(
        !stderr.contains("createIpResponse"),
        "private 関数は他ファイルの impact に紐付けてはならない: {stderr}"
    );
}

/// Rust の `pub use submodule::Foo;` で再エクスポートしているだけのファイルは、
/// エクスポート元シンボルの body-only 変更で impact に載せてはならない。
/// (R7 の TypeScript `export from` と同等、Rust 版の回帰ガード)
#[test]
fn impact_rust_pub_use_reexport_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let inner_rs = dir.path().join("inner.rs");
    let lib_rs = dir.path().join("lib.rs");

    std::fs::write(
        &inner_rs,
        r#"pub fn do_work(x: i32) -> i32 {
    x + 1
}
"#,
    )
    .unwrap();
    std::fs::write(
        &lib_rs,
        r#"pub mod inner;
pub use inner::do_work;
"#,
    )
    .unwrap();

    // do_work の body のみ変更 (シグネチャは不変)
    let diff = r#"--- a/inner.rs
+++ b/inner.rs
@@ -1,3 +1,4 @@
 pub fn do_work(x: i32) -> i32 {
-    x + 1
+    let y = x;
+    y + 1
 }
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected exit 0.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("lib.rs"),
        "`pub use` 再エクスポートのみの行は impact に載せてはならない: {stderr}"
    );
}

/// 新規クレートで `pub mod foo; pub mod bar;` のようなモジュール宣言のみを
/// 追加した場合、他クレート内で同名のローカル変数 (`tensor` / `ops` 等) が
/// impact に巻き添えで紐付かないことを確認する。モジュール名は
/// `should_include_for_cross_file` の段階で除外される。
/// (レポート triage-ocrus-nn-impact.md の再現)
#[test]
fn impact_module_declaration_no_cross_file_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let nn_dir = dir.path().join("crates/ocrus-nn/src");
    let cli_dir = dir.path().join("crates/ocrus-cli/src");
    std::fs::create_dir_all(&nn_dir).unwrap();
    std::fs::create_dir_all(&cli_dir).unwrap();

    std::fs::write(nn_dir.join("lib.rs"), "// empty\n").unwrap();
    // consumer 側に tensor / ops という名前のローカル変数 / クロージャ引数を持つコード
    std::fs::write(
        cli_dir.join("main.rs"),
        r#"fn char_accuracy() {
    let tensor = normalize_line();
    let _shape = tensor.shape();
    for (_i, tensor) in [1, 2].iter().enumerate() {
        let _ = tensor;
    }
    let ops: Vec<u8> = vec![];
    let _ = ops;
}
fn normalize_line() -> Vec<u8> { vec![] }
"#,
    )
    .unwrap();
    // 新規モジュール宣言を追加する diff
    let diff = r#"--- a/crates/ocrus-nn/src/lib.rs
+++ b/crates/ocrus-nn/src/lib.rs
@@ -1 +1,3 @@
-// empty
+pub mod arena;
+pub mod ops;
+pub mod tensor;
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected exit 0 (module 宣言追加は impact を出さない).\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("main.rs"),
        "consumer 側のローカル変数 tensor/ops は impact に載せてはならない: {stderr}"
    );
}

#[test]
fn impact_trait_unchanged_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern 1: trait definition is unchanged, but free functions using the trait
    // have signature changes. The trait name appears in changed lines but the trait
    // definition header (`trait GuestMemory`) is NOT changed.
    let dir = tempfile::tempdir().expect("tempdir");
    let mem_rs = dir.path().join("mem.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // mem.rs: trait + free function using the trait
    std::fs::write(
        &mem_rs,
        r#"pub trait GuestMemory {
    fn read(&self, addr: u64, buf: &mut [u8]);
    fn write(&self, addr: u64, data: &[u8]);
}

pub fn read_obj<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> u32 {
    let mut buf = [0u8; 4];
    mem.read(addr, &mut buf);
    u32::from_le_bytes(buf)
}
"#,
    )
    .unwrap();

    // consumer.rs: imports and uses GuestMemory
    std::fs::write(
        &consumer_rs,
        r#"use crate::mem::GuestMemory;

pub fn process(mem: &dyn GuestMemory) {
    let val = crate::mem::read_obj(mem, 0x1000);
    println!("{val}");
}
"#,
    )
    .unwrap();

    // Diff: only read_obj signature changes (dyn → impl + ?Sized), trait is unchanged
    let diff = r#"--- a/mem.rs
+++ b/mem.rs
@@ -5,7 +5,7 @@
 }

-pub fn read_obj(mem: &dyn GuestMemory, addr: u64) -> u32 {
+pub fn read_obj<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> u32 {
     let mut buf = [0u8; 4];
     mem.read(addr, &mut buf);
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // GuestMemory trait is NOT changed, so `use crate::mem::GuestMemory` imports
    // in consumer.rs should NOT be reported as impacted.
    // read_obj signature DID change, so read_obj callers may be reported.
    let ctx: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    if let Some(changes) = ctx["changes"].as_array() {
        for change in changes {
            // GuestMemory should NOT be in affected_symbols
            let empty = vec![];
            let affected = change["affected_symbols"].as_array().unwrap_or(&empty);
            let has_guest_memory = affected
                .iter()
                .any(|s| s["name"].as_str() == Some("GuestMemory"));
            assert!(
                !has_guest_memory,
                "GuestMemory trait should not be affected when its definition is unchanged.\nstdout: {stdout}\nstderr: {stderr}"
            );
        }
    }
}

#[test]
fn impact_test_symbols_excluded_from_affected() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern 2: test symbols (#[cfg(test)] mod tests) should not appear
    // in affected_symbols list.
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // lib.rs: pub fn + test module
    std::fs::write(
        &lib_rs,
        r#"pub fn compute(x: i32, y: i32) -> i32 {
    x * y + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> i32 {
        42
    }

    #[test]
    fn test_compute() {
        assert_eq!(compute(setup(), 2), 85);
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        &consumer_rs,
        r#"use crate::lib::compute;

pub fn run() -> i32 {
    compute(1, 2)
}
"#,
    )
    .unwrap();

    // Diff: changes both compute signature and test helper
    let diff = r#"--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
-pub fn compute(x: i32) -> i32 {
-    x + 1
+pub fn compute(x: i32, y: i32) -> i32 {
+    x * y + 1
 }
@@ -8,8 +8,8 @@

-    fn setup() -> i32 {
-        0
+    fn setup() -> i32 {
+        42
     }

     #[test]
-    fn test_compute() {
-        assert_eq!(compute(setup()), 1);
+    fn test_compute() {
+        assert_eq!(compute(setup(), 2), 85);
     }
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let ctx: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    if let Some(changes) = ctx["changes"].as_array() {
        for change in changes {
            let empty = vec![];
            let affected = change["affected_symbols"].as_array().unwrap_or(&empty);
            let test_symbols: Vec<&str> = affected
                .iter()
                .filter_map(|s| s["name"].as_str())
                .filter(|name| *name == "tests" || *name == "setup" || *name == "test_compute")
                .collect();
            assert!(
                test_symbols.is_empty(),
                "Test symbols should not appear in affected_symbols: {:?}\nstdout: {stdout}",
                test_symbols
            );
        }
    }
}

#[test]
fn impact_same_name_method_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern: same-name method across different types.
    // `Transport::write` changes should NOT be reported as impacting
    // `Device::write` references in another file.
    let dir = tempfile::tempdir().expect("tempdir");
    let transport_rs = dir.path().join("transport.rs");
    let device_rs = dir.path().join("device.rs");

    // transport.rs: impl Transport with write method
    std::fs::write(
        &transport_rs,
        r#"pub struct Transport {
    pub base: u64,
}

impl Transport {
    pub fn write(&mut self, offset: u64, value: u32) {
        // write to MMIO register
        let addr = self.base + offset;
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
    }
}
"#,
    )
    .unwrap();

    // device.rs: different trait with same-name method, plus usage of both
    std::fs::write(
        &device_rs,
        r#"pub trait Device {
    fn write(&mut self, offset: u64, size: u8, value: u64);
}

pub struct Keyboard;

impl Device for Keyboard {
    fn write(&mut self, offset: u64, size: u8, value: u64) {
        // handle keyboard write
    }
}

pub fn dispatch(dev: &mut dyn Device, offset: u64, value: u64) {
    dev.write(offset, 1, value);
}
"#,
    )
    .unwrap();

    // Diff: Transport::write signature changes
    let diff = r#"--- a/transport.rs
+++ b/transport.rs
@@ -5,7 +5,7 @@

 impl Transport {
-    pub fn write(&mut self, offset: u64, value: u32) {
+    pub fn write(&mut self, offset: u64, value: u32, mem: &[u8]) {
         // write to MMIO register
         let addr = self.base + offset;
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // device.rs has its own `write` definition (in trait Device).
    // Transport::write change should NOT impact Device::write references.
    assert!(
        !stdout.contains("device.rs"),
        "device.rs should not appear as impacted for Transport::write change.\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn impact_module_decl_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Fixture: lib.rs with module declarations, consumer.rs with same-name local variables
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // lib.rs: new crate with pub mod declarations
    std::fs::write(
        &lib_rs,
        r#"pub mod arena;
pub mod ops;
pub mod tensor;
"#,
    )
    .unwrap();

    // consumer.rs: uses "tensor" as a local variable name (unrelated crate)
    std::fs::write(
        &consumer_rs,
        r#"pub fn process() {
    let tensor = vec![1.0, 2.0, 3.0];
    let shape = tensor.len();
    println!("{shape}");
}
"#,
    )
    .unwrap();

    // Diff: adding new module declarations
    let diff = r#"--- /dev/null
+++ b/lib.rs
@@ -0,0 +1,3 @@
+pub mod arena;
+pub mod ops;
+pub mod tensor;
"#;

    let mut child = cargo_bin()
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Module declarations should NOT cause false positives on same-name local variables.
    assert!(
        output.status.success(),
        "expected exit 0 for module declarations.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("consumer.rs"),
        "consumer.rs should not appear as impacted for module declarations: {stdout}"
    );
}

/// Phase 4 設計バグの回帰テスト:
/// 同名 method (modified + added) が異なるファイルに存在するとき、
/// added 側 (新規追加ファイル) の fc_ix へ cross-file 参照が漏れないこと。
///
/// 旧実装: pass2 の `sym_to_fc` を **グローバル** `included_symbols.contains(sym_key)`
/// で判定していたため、Factory.php の `new` (modified) が include されるだけで、
/// Id.php (added) の `new` も同じ sym_key を持つため両 fc_ix に caller が流れていた。
///
/// 新実装: `FileContext.cross_file_symbol_keys` で per-file 判定するため、
/// added の Id 側には何も流れない。
#[test]
fn impact_per_file_routing_excludes_added_with_same_method_name() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let existing_rs = dir.path().join("existing.rs");
    let added_rs = dir.path().join("added.rs");
    let caller_rs = dir.path().join("caller.rs");

    // existing.rs: 既存ファイル (modified) — Existing::run シグネチャ変更後の状態
    std::fs::write(
        &existing_rs,
        r#"pub struct Existing;

impl Existing {
    pub fn run(&self, x: u32) -> u32 { x + 1 }
}
"#,
    )
    .unwrap();

    // added.rs: 新規ファイル (added) — Added::run も同名メソッドを持つ
    std::fs::write(
        &added_rs,
        r#"pub struct Added;

impl Added {
    pub fn run(&self) -> u32 { 0 }
}
"#,
    )
    .unwrap();

    // caller.rs: Existing::run の caller がいる (Added は使わない)
    std::fs::write(
        &caller_rs,
        r#"use crate::existing::Existing;

pub fn use_existing(e: &Existing) -> u32 {
    e.run(42)
}
"#,
    )
    .unwrap();

    // diff: existing.rs を modify + added.rs を新規追加
    let diff = r#"--- a/existing.rs
+++ b/existing.rs
@@ -2,4 +2,4 @@
 pub struct Existing;

 impl Existing {
-    pub fn run(&self) -> u32 { 1 }
+    pub fn run(&self, x: u32) -> u32 { x + 1 }
 }
--- /dev/null
+++ b/added.rs
@@ -0,0 +1,5 @@
+pub struct Added;
+
+impl Added {
+    pub fn run(&self) -> u32 { 0 }
+}
"#;

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "context failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("changes array missing: {stdout}"));

    // added.rs の change を取り出して impacted_callers が空であることを確認。
    let added_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("added.rs"))
        .unwrap_or_else(|| {
            panic!("added.rs change entry not found in: {stdout}");
        });
    let added_callers = added_change
        .get("impacted_callers")
        .and_then(|c| c.as_array());
    assert!(
        added_callers.is_none_or(|c| c.is_empty()),
        "added.rs (added file) must have no impacted_callers, but got: {added_change}"
    );
    let added_low = added_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array());
    assert!(
        added_low.is_none_or(|c| c.is_empty()),
        "added.rs (added file) must have no low_confidence_callers, but got: {added_change}"
    );
}

/// PHP trait の親型認識テスト (load-bearing バグの回帰テスト):
/// `trait Factory { public static function new() }` のような trait scope の
/// メソッドが変更された際、Stage 4b の parent_in_this_file チェックが
/// 効くようにするため、`trait_declaration` が親型として認識されること。
///
/// 旧実装は `class_declaration` だけを親型として認識していたため、PHP trait
/// 内の同名メソッド (`new` 等) で `parent_ix_by_sym = None` となり、Stage 4b が
/// 完全にバイパスされ、`Other::new()` 系の同名 method 全件が誤って
/// impacted_callers に流れていた。
#[test]
fn impact_php_trait_method_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let trait_php = dir.path().join("Factory.php");
    let other_php = dir.path().join("Other.php");
    let unrelated_php = dir.path().join("Unrelated.php");

    // Factory.php: trait scope の static method `new`
    std::fs::write(
        &trait_php,
        r#"<?php
namespace App\Factories;

trait Factory {
    public static function new(int $x): self {
        return new self($x + 1);
    }
}
"#,
    )
    .unwrap();

    // Other.php: 別 class の同名 static method `new`
    std::fs::write(
        &other_php,
        r#"<?php
namespace App\Other;

class Other {
    public static function new(): self {
        return new self();
    }
}
"#,
    )
    .unwrap();

    // Unrelated.php: Other::new() を呼ぶが Factory trait は use していない
    std::fs::write(
        &unrelated_php,
        r#"<?php
namespace App\Consumers;

use App\Other\Other;

class Consumer {
    public function consume(): void {
        $obj = Other::new();
    }
}
"#,
    )
    .unwrap();

    // diff: Factory trait の new シグネチャを変更
    let diff = r#"--- a/Factory.php
+++ b/Factory.php
@@ -3,7 +3,7 @@
 namespace App\Factories;

 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x + 1);
     }
 }
"#;

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let factory_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Factory.php"))
        .unwrap_or_else(|| panic!("Factory.php change not found: {stdout}"));

    let impacted = factory_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = factory_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    // Unrelated.php は Other::new() を呼ぶだけで Factory trait は触らない。
    // trait_declaration が親型認識されれば parent_in_this_file=false で skip される。
    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("Unrelated.php"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("Unrelated.php"));
    assert!(
        !unrelated_in_impacted,
        "Unrelated.php must NOT appear in impacted_callers (Stage 4b parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "Unrelated.php must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// Rust trait_item 親型認識の回帰テスト。
///
/// PHP の trait_declaration と同様、Rust の `trait Foo { fn bar() {} }` も
/// 親型認識されないと Stage 4b parent_in_this_file が常に false になり、
/// 別 struct の同名 method が impacted_callers に流れる。
#[test]
fn impact_rust_trait_item_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let trait_rs = dir.path().join("factory.rs");
    let other_rs = dir.path().join("other.rs");
    let unrelated_rs = dir.path().join("unrelated.rs");

    // factory.rs: trait scope の method `new`
    std::fs::write(
        &trait_rs,
        r#"pub trait Factory {
    fn new(x: i32) -> Self
    where
        Self: Sized;
}
"#,
    )
    .unwrap();

    // other.rs: 別 struct の同名 method `new`
    std::fs::write(
        &other_rs,
        r#"pub struct Other;

impl Other {
    pub fn new() -> Self {
        Other
    }
}
"#,
    )
    .unwrap();

    // unrelated.rs: Other::new() を呼ぶだけ。Factory trait は触らない。
    std::fs::write(
        &unrelated_rs,
        r#"use crate::other::Other;

pub fn consume() {
    let _ = Other::new();
}
"#,
    )
    .unwrap();

    // diff: Factory trait の `new` シグネチャを変更
    let diff = r#"--- a/factory.rs
+++ b/factory.rs
@@ -1,5 +1,5 @@
 pub trait Factory {
-    fn new(x: i32) -> Self
+    fn new(x: i32, y: i32) -> Self
     where
         Self: Sized;
 }
"#;

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let factory_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("factory.rs"))
        .unwrap_or_else(|| panic!("factory.rs change not found: {stdout}"));

    let impacted = factory_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = factory_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.rs"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.rs"));
    assert!(
        !unrelated_in_impacted,
        "unrelated.rs must NOT appear in impacted_callers (Rust trait_item parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "unrelated.rs must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// TypeScript abstract_class_declaration 親型認識の回帰テスト。
///
/// `abstract class Foo { abstract bar(): void }` は通常の class_declaration ではなく
/// abstract_class_declaration ノードになるため、別途認識を追加しないと parent が消える。
#[test]
fn impact_typescript_abstract_class_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let abs_ts = dir.path().join("base.ts");
    let other_ts = dir.path().join("other.ts");
    let unrelated_ts = dir.path().join("unrelated.ts");

    // base.ts: abstract class scope の method `process`。
    // 本体のない abstract シグネチャは affected symbol にならないため、
    // 具象メソッドを abstract class 内に置いて parent 認識 (abstract_class_declaration)
    // を実際に効かせる (空 FileImpact はスキップされるようになったため、
    // affected が空だとフィルタ検証自体が空振りする)。
    std::fs::write(
        &abs_ts,
        r#"export abstract class Base {
    process(x: number): number {
        return x;
    }
}
"#,
    )
    .unwrap();

    // other.ts: 別 class の同名 method `process`
    std::fs::write(
        &other_ts,
        r#"export class Other {
    process(): number {
        return 0;
    }
}
"#,
    )
    .unwrap();

    // unrelated.ts: Other.process() を呼ぶだけ
    std::fs::write(
        &unrelated_ts,
        r#"import { Other } from "./other";

export function consume(): number {
    const o = new Other();
    return o.process();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/base.ts
+++ b/base.ts
@@ -1,5 +1,5 @@
 export abstract class Base {
-    process(x: number): number {
+    process(x: number, y: number): number {
         return x;
     }
 }
"#;

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let base_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("base.ts"))
        .unwrap_or_else(|| panic!("base.ts change not found: {stdout}"));

    let impacted = base_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = base_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.ts"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.ts"));
    assert!(
        !unrelated_in_impacted,
        "unrelated.ts must NOT appear in impacted_callers (TS abstract_class_declaration parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "unrelated.ts must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// impact のデフォルト除外ディレクトリ動作確認。
///
/// vendor / node_modules / target などの 3rd-party / build artifact ディレクトリ内に
/// 同名メソッドが置かれていても、`impacted_callers` には流れ込まないこと。
#[test]
fn impact_default_excluded_dirs_drops_vendor_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let owner_php = dir.path().join("Owner.php");
    let vendor_php = dir.path().join("vendor").join("Caller.php");
    let target_rs_dir = dir.path().join("target").join("debug");
    std::fs::create_dir_all(target_rs_dir.parent().unwrap()).unwrap();
    std::fs::create_dir_all(vendor_php.parent().unwrap()).unwrap();

    // Owner.php: trait scope の static method `new`
    std::fs::write(
        &owner_php,
        r#"<?php
trait Factory {
    public static function new(int $x): self {
        return new self($x);
    }
}
"#,
    )
    .unwrap();

    // vendor/Caller.php: 同名 static method `new` を持つ別 class
    std::fs::write(
        &vendor_php,
        r#"<?php
class VendorThing {
    public static function new(): self {
        return new self();
    }
}
function consume_vendor(): void {
    $obj = VendorThing::new();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/Owner.php
+++ b/Owner.php
@@ -1,5 +1,5 @@
 <?php
 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x);
     }
 }
"#;

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let owner_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Owner.php"))
        .unwrap_or_else(|| panic!("Owner.php change not found: {stdout}"));

    let impacted = owner_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = owner_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let vendor_in_impacted = impacted.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("vendor"))
            .unwrap_or(false)
    });
    let vendor_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("vendor"))
            .unwrap_or(false)
    });

    assert!(
        !vendor_in_impacted,
        "vendor/ caller must NOT appear in impacted_callers. impacted: {impacted:?}"
    );
    assert!(
        !vendor_in_low,
        "vendor/ caller must NOT appear in low_confidence_callers. low: {low:?}"
    );
}

/// `--exclude-dir` が impact 解析の cross-file 検索でも作用する回帰テスト (v26.5.117)。
///
/// `IMPACT_DEFAULT_EXCLUDED_DIRS` の固定リストに含まれない命名 (バージョン入りの
/// `pjproject-2.15` 等) でも、ユーザーが `--exclude-dir` で渡せば impact から除外される。
#[test]
fn impact_user_exclude_dir_drops_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let owner_php = dir.path().join("Owner.php");
    let custom_dir = dir.path().join("pjproject-2.15");
    std::fs::create_dir_all(&custom_dir).unwrap();
    let custom_caller = custom_dir.join("Caller.php");

    std::fs::write(
        &owner_php,
        r#"<?php
trait Factory {
    public static function new(int $x): self {
        return new self($x);
    }
}
"#,
    )
    .unwrap();
    // 命名が IMPACT_DEFAULT_EXCLUDED_DIRS に含まれないので、デフォルトでは impact 対象。
    std::fs::write(
        &custom_caller,
        r#"<?php
class CustomThing {
    public static function new(): self {
        return new self();
    }
}
function consume_custom(): void {
    $obj = CustomThing::new();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/Owner.php
+++ b/Owner.php
@@ -1,5 +1,5 @@
 <?php
 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x);
     }
 }
"#;

    // ユーザーが --exclude-dir pjproject-2.15 を渡せば、impact からも除外される。
    let mut child = cargo_bin()
        .args([
            "context",
            "--dir",
            dir.path().to_str().unwrap(),
            "--exclude-dir",
            "pjproject-2.15",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let owner_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Owner.php"))
        .unwrap_or_else(|| panic!("Owner.php change not found: {stdout}"));

    let impacted = owner_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = owner_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let custom_in_impacted = impacted.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("pjproject-2.15"))
            .unwrap_or(false)
    });
    let custom_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("pjproject-2.15"))
            .unwrap_or(false)
    });
    assert!(
        !custom_in_impacted,
        "pjproject-2.15/ caller must NOT appear in impacted_callers when --exclude-dir is set. impacted: {impacted:?}"
    );
    assert!(
        !custom_in_low,
        "pjproject-2.15/ caller must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// 不正な `--exclude-glob` (構文エラー) がエラーで終了することを確認する。
///
/// silent empty 結果 (= 全 impact が消える) の方がユーザーにとって危険なので、
/// `validate_exclude_globs` で先行検証して `INVALID_REQUEST` で落とす。
#[test]
fn impact_invalid_exclude_glob_returns_error() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("foo.rs"), "fn x() {}\n").unwrap();
    let diff = r#"--- a/foo.rs
+++ b/foo.rs
@@ -1,1 +1,1 @@
-fn x() {}
+fn x(y: i32) {}
"#;

    let mut child = cargo_bin()
        .args([
            "context",
            "--dir",
            dir.path().to_str().unwrap(),
            "--exclude-glob",
            "[",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "invalid --exclude-glob should fail. stdout: {stdout}"
    );
    assert!(
        stdout.contains("INVALID_REQUEST") || stdout.contains("invalid exclude-glob"),
        "expected JSON error mentioning invalid exclude-glob. got: {stdout}"
    );
}

// ---- impact パストラバーサル検証テスト ----

#[test]
fn impact_rejects_path_traversal_in_diff() {
    use std::io::Write;
    use std::process::Stdio;

    // diff パス内の .. はスキップされるべき
    let diff = r#"--- a/../../../etc/passwd
+++ b/../../../etc/passwd
@@ -1,3 +1,3 @@
-root:x:0:0:root
+root:x:0:0:hacked
"#;

    let dir = tempfile::tempdir().expect("tempdir");

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(diff.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let changes = json["changes"].as_array().unwrap();
    assert!(
        changes.is_empty(),
        "パストラバーサルを含む diff は変更として認識されないべき"
    );
}

#[test]
fn impact_routes_import_only_refs_to_informational_callers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.ts"),
        "export function compute(input: number, offset: number = 0): number {\n  return input + offset;\n}\n",
    )
    .expect("write changed file");
    std::fs::write(
        src_dir.join("import_only.ts"),
        "import { compute } from './lib';\nexport const label = 'only import';\n",
    )
    .expect("write import-only file");
    std::fs::write(
        src_dir.join("barrel.ts"),
        "import { compute } from './lib';\n  export { compute };\n",
    )
    .expect("write barrel file");
    std::fs::write(
        src_dir.join("barrel_call.ts"),
        "import { compute } from './lib';\n  export { compute }; export const value = compute(1);\n",
    )
    .expect("write barrel call file");
    std::fs::write(
        src_dir.join("caller.ts"),
        "import { compute } from './lib';\nexport const value = compute(1);\n",
    )
    .expect("write caller file");

    let diff = "\
diff --git a/src/lib.ts b/src/lib.ts\n\
--- a/src/lib.ts\n\
+++ b/src/lib.ts\n\
@@ -1,3 +1,3 @@\n\
-export function compute(input: number): number {\n\
-  return input;\n\
+export function compute(input: number, offset: number = 0): number {\n\
+  return input + offset;\n\
 }\n";
    let service = astro_sight::service::AppService::new();
    let result = service
        .analyze_context(
            diff,
            tmp.path().to_str().expect("utf-8 path"),
            &astro_sight::models::impact::ContextAnalysisOptions::default(),
        )
        .expect("analyze_context should succeed");
    let change = result
        .changes
        .iter()
        .find(|c| c.path == "src/lib.ts")
        .expect("changed file impact should be present");

    assert!(
        change.impacted_callers.iter().any(|caller| {
            caller.path.ends_with("src/caller.ts") && caller.symbols == vec!["compute".to_string()]
        }),
        "actual call should remain in impacted_callers: {:?}",
        change.impacted_callers
    );
    assert!(
        change.impacted_callers.iter().any(|caller| {
            caller.path.ends_with("src/barrel_call.ts")
                && caller.symbols == vec!["compute".to_string()]
        }),
        "same-line call after re-export should remain in impacted_callers: {:?}",
        change.impacted_callers
    );
    assert!(
        change.informational_callers.iter().any(|caller| {
            caller.path.ends_with("src/import_only.ts")
                && caller.symbols == vec!["compute".to_string()]
                && caller.confidence.as_deref() == Some("informational")
        }),
        "import-only ref should be informational: {:?}",
        change.informational_callers
    );
    assert!(
        change.informational_callers.iter().any(|caller| {
            caller.path.ends_with("src/barrel.ts")
                && caller.symbols == vec!["compute".to_string()]
                && caller.confidence.as_deref() == Some("informational")
        }),
        "barrel re-export should be informational: {:?}",
        change.informational_callers
    );
    assert!(
        change
            .informational_callers
            .iter()
            .all(|caller| std::path::Path::new(&caller.path).is_relative()),
        "informational paths should stay workspace-relative: {:?}",
        change.informational_callers
    );
    assert!(
        !change
            .impacted_callers
            .iter()
            .any(|caller| caller.path.ends_with("src/import_only.ts")),
        "import-only ref must not be mixed into impacted_callers"
    );
    assert!(
        !change
            .impacted_callers
            .iter()
            .any(|caller| caller.path.ends_with("src/barrel.ts")),
        "barrel re-export must not be mixed into impacted_callers"
    );
}

// ---- impact --hook フラグテスト ----

#[test]
fn impact_hook_shows_triage_message() {
    // 変更のある diff を使って impact を実行し、--hook 時にトリアージメッセージが出力されることを確認
    let tmp_dir = tempfile::tempdir().unwrap();
    let src_dir = tmp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        src_dir.join("main.rs"),
        "use crate::lib::compute;\nfn main() { compute(1); }\n",
    )
    .unwrap();

    // compute の署名変更 diff
    let diff = r#"--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-pub fn compute(x: i32) -> i32 { x + 1 }
+pub fn compute(x: i32, y: i32) -> i32 { x + y }
"#;

    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            tmp_dir.path().to_str().unwrap(),
            "--hook",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(diff.as_bytes())
                .unwrap();
            child.wait_with_output()
        })
        .expect("failed to run");

    // 未解決の影響がある場合、exit 1 で --hook メッセージが出る
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("astro-sight-triage"),
            "--hook 時にトリアージスキルの案内が表示されるべき: {}",
            stderr
        );
    }
    // 未解決影響がない場合（main.rs が解析できない等）は exit 0 で問題なし
}

/// TS/TSX 名前衝突 false positive 抑制 (Issue 2026-06-05-multi-attachment-conversations-fp):
/// `schema.ts` を import していない `ConversationList.tsx` の props 変数 `conversations`
/// (Drizzle table と同名) は `impacted_callers` ではなく `low_confidence_callers` に振り分け
/// られて Stop hook の blocking から外れる。
#[test]
fn impact_ts_name_collision_without_direct_import_routed_to_low_confidence() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("lib/db")).unwrap();
    std::fs::create_dir_all(dir.path().join("components")).unwrap();

    // schema.ts: Drizzle table export
    let schema_path = dir.path().join("lib/db/schema.ts");
    std::fs::write(&schema_path, "export const conversations = { id: 0 };\n").unwrap();

    // ConversationList.tsx: schema.ts を import せず、独自の interface + destructured props
    let tsx_path = dir.path().join("components/ConversationList.tsx");
    std::fs::write(
        &tsx_path,
        r#"interface Conversation { id: number }
interface Props { conversations: Conversation[] }
export function ConversationList({ conversations }: Props) {
    return conversations.length;
}
"#,
    )
    .unwrap();

    // diff: schema.ts の conversations を変更
    let diff = r#"--- a/lib/db/schema.ts
+++ b/lib/db/schema.ts
@@ -1 +1 @@
-export const conversations = { id: 0 };
+export const conversations = { id: 0, title: "" };
"#;

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let schema_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("lib/db/schema.ts"))
        .unwrap_or_else(|| panic!("schema.ts change not found: {stdout}"));
    let impacted = schema_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = schema_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    // ConversationList.tsx は schema.ts を import していないため、impacted_callers には
    // 出さず low_confidence_callers (informational) に振り分けるべき。Stop hook blocking から
    // 外しつつ Information として残す。
    let tsx_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("components/ConversationList.tsx"));
    assert!(
        !tsx_in_impacted,
        "ConversationList.tsx must NOT appear in impacted_callers (no direct import of schema.ts). got: {impacted:?} | low: {low:?}"
    );
    // 低 confidence の振り分け先に出ることも assert (codex 助言: low 出力自体を仕様化)。
    // path は absolute (tempdir) になりうるので末尾マッチで判定する。
    let tsx_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .is_some_and(|s| s.ends_with("components/ConversationList.tsx"))
    });
    assert!(
        tsx_in_low,
        "ConversationList.tsx は low_confidence_callers に出るべき (informational として残す)。got low: {low:?}"
    );
}

/// 直接 import している場合は high impact (`impacted_callers`) に残る (逆方向の回帰テスト):
/// `ConversationList.tsx` が `schema.ts` を直接 import している場合は従来通り
/// `impacted_callers` に出る。
#[test]
fn impact_ts_name_collision_with_direct_import_stays_high() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("lib/db")).unwrap();
    std::fs::create_dir_all(dir.path().join("components")).unwrap();

    let schema_path = dir.path().join("lib/db/schema.ts");
    std::fs::write(&schema_path, "export const conversations = { id: 0 };\n").unwrap();

    // ConversationList.tsx: schema.ts を直接 import する
    let tsx_path = dir.path().join("components/ConversationList.tsx");
    std::fs::write(
        &tsx_path,
        r#"import { conversations } from "../lib/db/schema";
export function getList() {
    return conversations;
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/lib/db/schema.ts
+++ b/lib/db/schema.ts
@@ -1 +1 @@
-export const conversations = { id: 0 };
+export const conversations = { id: 0, title: "" };
"#;

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let schema_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("lib/db/schema.ts"))
        .unwrap_or_else(|| panic!("schema.ts change not found: {stdout}"));
    let impacted = schema_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let tsx_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("components/ConversationList.tsx"));
    assert!(
        tsx_in_impacted,
        "ConversationList.tsx は schema.ts を直接 import しているため impacted_callers に出るべき。got: {impacted:?}"
    );
}

#[test]
fn impact_fn_value_passed_ref_is_informational_for_signature_only_change() {
    // Issue 2026-07-12-bevy-systemparam-optional-res-impact-fp:
    // pub fn の引数型のみ変更 (arity 不変) のとき、タプル要素として値渡しされる
    // だけの参照 (callee でない) は unresolved impact に出ない (informational 格下げ)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"sysparam-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod reg;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Res<u32>) {\n    let _ = r;\n}\npub fn other_a() {}\npub fn other_b() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/reg.rs"),
        "use crate::sys::{my_system, other_a, other_b};\n\npub fn register<F>(_f: F) {}\n\npub fn setup() {\n    register((other_a, my_system, other_b));\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // Res<u32> -> Option<Res<u32>> (arity 不変の型のみ変更)
    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Option<Res<u32>>) {\n    let _ = r;\n}\npub fn other_a() {}\npub fn other_b() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "値渡し参照のみなら unresolved impact を出さず exit 0: {stderr}"
    );
}

#[test]
fn impact_fn_callee_ref_stays_blocking_for_signature_change() {
    // 対照: callee として呼び出している参照はシグネチャ変更で blocking のまま。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"callee-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod caller;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Res<u32>) {\n    let _ = r;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/caller.rs"),
        "use crate::sys::{Res, my_system};\n\npub fn drive() {\n    my_system(Res(1));\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Option<Res<u32>>) {\n    let _ = r;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "callee 参照はシグネチャ変更で blocking (exit 1) のまま"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("caller.rs:4"),
        "人間向け表示はエディタと同じ 1-indexed 行番号: {stderr}"
    );
}

#[test]
fn impact_fn_direct_argument_ref_stays_blocking() {
    // codex レビュー指摘 (重大1): 直接引数 (`accept(my_system)`) は呼び出し先が
    // `fn accept(_: fn(u32))` のような fn ポインタ引数を取る場合に型変更で壊れる。
    // AST だけでは渡し先シグネチャを解決できないため、タプル/配列要素と違い
    // blocking を維持する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"directarg-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod reg;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: u32) {\n    let _ = x;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/reg.rs"),
        "use crate::sys::my_system;\n\npub fn accept(_f: fn(u32)) {}\n\npub fn setup() {\n    accept(my_system);\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // u32 -> String (arity 不変の型のみ変更) — fn ポインタ引数へ渡しているため壊れる
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: String) {\n    let _ = x;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "直接引数の関数値渡しは blocking (exit 1) を維持すべき"
    );
}

#[test]
fn impact_fn_typed_tuple_ref_stays_blocking() {
    // codex 再レビュー指摘 (重大1): `let handlers: (fn(u32),) = (my_system,);` のように
    // タプル/配列の外側に明示型注釈がある場合、要素は fn ポインタ型に固定され
    // シグネチャ変更 (arity 不変でも) でコンパイルエラーになる。informational へ
    // 格下げせず blocking を維持する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"typedtuple-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod reg;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: u32) {\n    let _ = x;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/reg.rs"),
        "use crate::sys::my_system;\n\npub fn setup() {\n    let handlers: (fn(u32),) = (my_system,);\n    let _ = handlers;\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // u32 -> String (arity 不変の型のみ変更) — fn ポインタ型固定のため壊れる
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: String) {\n    let _ = x;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "明示型付きタプル要素の関数値渡しは blocking (exit 1) を維持すべき"
    );
}

/// Issue 2026-08-07-reexport-treated-as-unresolved-impact:
/// 定義を別モジュールへ移動し、旧公開パスを `pub use` で維持すると、旧パス側に残る
/// **未変更の利用行**が「未解決の影響」として blocking 報告されていた。
/// 同一 diff 内で名前解決が完結しているため informational へ降格する。
#[test]
fn review_hook_reexport_move_is_informational_not_blocking() {
    let base_background = "pub const BASE_Y: f32 = -70.0;\npub const BAND_H: f32 = 30.0;\n\npub fn draw() -> f32 {\n    BASE_Y + BAND_H\n}\n";
    // consumer が CONST_A を自前で定義し、同ファイル内で利用している。
    let base_consumer = "pub const CONST_A: f32 = -50.0;\n\npub struct Item {\n    pub size: f32,\n}\n\nimpl Item {\n    pub fn placement(&self) -> f32 {\n        CONST_A + self.size * 0.5\n    }\n}\n";

    let output = a3_review_hook(
        |root| {
            // CONST_A の定義を background へ移動。BAND_H の値も同時に変更することで
            // 「削除行を含む混在 hunk」になり、CONST_A は added ではなく modified 判定になる
            // (これが Pass1 のフィルタ 6 をすり抜けて cross-file 探索に載る条件)。
            std::fs::write(
                root.join("src/game/background.rs"),
                "pub const BASE_Y: f32 = -70.0;\npub const BAND_H: f32 = 40.0;\npub const CONST_A: f32 = BASE_Y + BAND_H * 0.5;\n\npub fn draw() -> f32 {\n    BASE_Y + BAND_H\n}\n",
            )
            .unwrap();
            // 旧公開パスは再輸出で維持。利用行 (placement) は無変更。
            std::fs::write(
                root.join("src/game/consumer.rs"),
                "pub use super::background::CONST_A;\n\npub struct Item {\n    pub size: f32,\n}\n\nimpl Item {\n    pub fn placement(&self) -> f32 {\n        CONST_A + self.size * 0.5\n    }\n}\n",
            )
            .unwrap();
        },
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", "pub mod game;\n"),
            (
                "src/game/mod.rs",
                "pub mod background;\npub mod consumer;\n",
            ),
            ("src/game/background.rs", base_background),
            ("src/game/consumer.rs", base_consumer),
        ],
    );

    // `review --hook` の JSON は stderr に出る。
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "再輸出で解決済みの移動は hook を落とすべきではない (exit 0 期待)\nstderr: {stderr}"
    );
    assert!(
        !stderr.trim().is_empty(),
        "hook 出力が空 (解析自体が skip された可能性)"
    );
    let json: serde_json::Value = serde_json::from_str(stderr.trim()).expect("hook json");
    assert!(
        json.get("impacts").is_none(),
        "blocking な impacts は空であるべき: {stderr}"
    );
    // 情報としては残す (完全な沈黙にはしない)。移動元の未変更利用行も含めて
    // informational へ回っていることを確認する。
    let info = json
        .get("impact_info")
        .and_then(|v| v.as_array())
        .expect("informational として報告は残すべき");
    let refs = info[0]["refs"].as_array().expect("refs");
    assert!(
        refs.iter()
            .any(|r| r["p"] == "src/game/consumer.rs" && r["ln"] == 8),
        "無変更の利用行 (consumer.rs:8) が informational に含まれるべき: {stderr}"
    );
}

/// 対照ケース: 上の抑制が効きすぎていないことの確認。
/// **`review_hook_reexport_move_is_informational_not_blocking` と diff の形を揃え、
/// 移動先の宣言の型だけを `f32` → `f64` に変える**。再輸出があっても参照側の式は
/// 型不一致で壊れるため blocking を維持しなければならない。
///
/// 対照を「引数を増やした関数の移動」にしてはいけない: 関数はフィルタ 3
/// (同一ファイル内で `-`/`+` 両方のシグネチャ行が要る `sig_changes` 判定) で
/// cross-file 探索から落ち、抑制の有無に関わらず impact が出ない = 別の理由で
/// テストが通ってしまう。
#[test]
fn review_hook_reexport_move_with_type_change_stays_blocking() {
    let base_background = "pub const BASE_Y: f32 = -70.0;\npub const BAND_H: f32 = 30.0;\n\npub fn draw() -> f32 {\n    BASE_Y + BAND_H\n}\n";
    let base_consumer = "pub const CONST_A: f32 = -50.0;\n\npub struct Item {\n    pub size: f32,\n}\n\nimpl Item {\n    pub fn placement(&self) -> f32 {\n        CONST_A + self.size * 0.5\n    }\n}\n";

    let output = a3_review_hook(
        |root| {
            // 移動先の宣言の型が f32 → f64 に変わる (宣言ヘッダが不一致)。
            std::fs::write(
                root.join("src/game/background.rs"),
                "pub const BASE_Y: f32 = -70.0;\npub const BAND_H: f32 = 40.0;\npub const CONST_A: f64 = -50.0;\n\npub fn draw() -> f32 {\n    BASE_Y + BAND_H\n}\n",
            )
            .unwrap();
            std::fs::write(
                root.join("src/game/consumer.rs"),
                "pub use super::background::CONST_A;\n\npub struct Item {\n    pub size: f32,\n}\n\nimpl Item {\n    pub fn placement(&self) -> f32 {\n        CONST_A + self.size * 0.5\n    }\n}\n",
            )
            .unwrap();
        },
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", "pub mod game;\n"),
            (
                "src/game/mod.rs",
                "pub mod background;\npub mod consumer;\n",
            ),
            ("src/game/background.rs", base_background),
            ("src/game/consumer.rs", base_consumer),
        ],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "型が変わった移動は blocking を維持すべき (exit 1 期待)\nstderr: {stderr}"
    );
    let json: serde_json::Value = serde_json::from_str(stderr.trim()).expect("hook json");
    let impacts = json
        .get("impacts")
        .and_then(|v| v.as_array())
        .expect("blocking な impacts が残るべき");
    let refs = impacts[0]["refs"].as_array().expect("refs");
    assert!(
        refs.iter()
            .any(|r| r["p"] == "src/game/consumer.rs" && r["ln"] == 8),
        "未変更の利用行が blocking 側に残るべき: {stderr}"
    );
}

#[test]
fn impact_removed_reexport_replaced_by_incompatible_type_stays_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        "pub mod api;\npub mod inner;\npub mod user;\n",
    );
    repo.write("src/inner.rs", "pub struct Shape(pub u8);\n");
    repo.write(
        "src/api.rs",
        "pub use crate::inner::Shape;\npub fn changed() -> u8 { 1 }\n",
    );
    repo.write(
        "src/user.rs",
        "use crate::api::Shape;\npub fn consume() -> Shape { Shape(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/api.rs",
        "pub struct Shape(pub String);\npub fn changed() -> u8 { 2 }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/api.rs")
        .unwrap();
    assert!(
        change["affected_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "Shape" && symbol["change_type"] == "modified"),
        "{context}"
    );
    assert!(
        change["impacted_callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caller| caller["path"] == "src/user.rs"),
        "{context}"
    );
    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "破壊的な型置換は blocking であるべき: {context}"
    );
}

#[test]
fn impact_grouped_use_removal_and_unchanged_glob_stay_blocking() {
    for (base_api, new_api) in [
        (
            "pub use crate::inner::{\n    Other,\n    Shape,\n};\npub fn changed() -> u8 { 1 }\n",
            "pub use crate::inner::{\n    Other,\n};\npub struct Shape(pub String);\npub fn changed() -> u8 { 2 }\n",
        ),
        (
            "pub use crate::inner::*;\npub fn changed() -> u8 { 1 }\n",
            "pub use crate::inner::*;\npub struct Shape(pub String);\npub fn changed() -> u8 { 2 }\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "src/lib.rs",
            "pub mod api;\npub mod inner;\npub mod user;\n",
        );
        repo.write(
            "src/inner.rs",
            "pub struct Shape(pub u8);\npub struct Other;\n",
        );
        repo.write("src/api.rs", base_api);
        repo.write(
            "src/user.rs",
            "use crate::api::Shape;\npub fn consume() -> Shape { Shape(1) }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/api.rs", new_api);

        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/api.rs")
            .unwrap();
        assert!(
            change["affected_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["name"] == "Shape" && symbol["change_type"] == "modified"),
            "{context}"
        );
        let output = cargo_bin()
            .args([
                "impact",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{context}");
    }
}

#[test]
fn impact_deleted_rust_file_keeps_moved_name_modified() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("src/lib.rs", "pub mod api;\npub mod old;\npub mod user;\n");
    repo.write("src/old.rs", "pub struct Shape(pub u8);\n");
    repo.write("src/api.rs", "pub fn changed() -> u8 { 1 }\n");
    repo.write(
        "src/user.rs",
        "use crate::old::Shape;\npub fn consume() -> Shape { Shape(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    std::fs::remove_file(repo.root().join("src/old.rs")).unwrap();
    repo.write(
        "src/api.rs",
        "pub struct Shape(pub String);\npub fn changed() -> u8 { 2 }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/api.rs")
        .unwrap();
    assert!(
        change["affected_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "Shape" && symbol["change_type"] == "modified"),
        "{context}"
    );
}

#[test]
fn complementary_rust_cfg_constants_have_one_public_record() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("src/lib.rs", "pub fn baseline() {}\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        "pub fn baseline() {}\n\
         #[cfg(target_os = \"macos\")]\n\
         pub const MESSAGE: &str = \"a\";\n\
         #[cfg(not(target_os = \"macos\"))]\n\
         pub const MESSAGE: &str = \"b\";\n\
         #[cfg(feature = \"alternate\")]\n\
         pub const DIFFERENT_TYPE: u8 = 1;\n\
         #[cfg(not(feature = \"alternate\"))]\n\
         pub const DIFFERENT_TYPE: u16 = 2;\n\
         #[cfg(feature = \"one\")]\n\
         const NESTED: u8 = 1;\n\
         #[cfg(not(feature = \"one\"))]\n\
         const NESTED: u8 = 2;\n\
         pub mod first { pub const NESTED: u8 = 3; }\n\
         pub mod second { pub const NESTED: u8 = 4; }\n",
    );

    let review = repo.run_json("review", &["--git"]);
    let added = review["api_changes"]["added"].as_array().unwrap();
    let affected = review["impact"]["changes"][0]["affected_symbols"]
        .as_array()
        .unwrap();
    for (name, expected_api, expected_impact) in [
        ("MESSAGE", 1, 1),
        ("DIFFERENT_TYPE", 2, 2),
        ("NESTED", 2, 4),
    ] {
        assert_eq!(
            added.iter().filter(|symbol| symbol["name"] == name).count(),
            expected_api,
            "{review}"
        );
        assert_eq!(
            affected
                .iter()
                .filter(|symbol| symbol["name"] == name)
                .count(),
            expected_impact,
            "{review}"
        );
    }
}

#[test]
fn impact_rust_impl_method_additions_do_not_change_the_owner_type() {
    let repo = TestRepo::new();
    repo.write(
        "lib.rs",
        "pub struct Foo;\nimpl Foo {\n    pub fn stable(&self) {}\n}\n\n\
         pub struct Bar;\n\n\
         pub trait Contract { fn required(&self); }\n\n\
         pub struct Holder<T>(pub T);\n\n\
         pub fn other() -> u8 { 1 }\n",
    );
    repo.write(
        "consumer.rs",
        "use crate::Foo;\npub fn make() -> Foo { Foo }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "lib.rs",
        "pub struct Foo;\nimpl Foo {\n    pub fn stable(&self) {}\n    /// A new operation.\n    #[inline]\n    pub fn fresh(&self) {}\n}\n\n\
         pub struct Bar;\n\n\
         impl Bar { pub fn bar_method(&self) {} }\n\n\
         pub trait Contract { fn required(&self); }\n\n\
         impl Contract for Bar { fn required(&self) {} }\n\n\
         pub struct Holder<T>(pub T);\n\n\
         impl<T> Holder<T> { pub fn generic_method(&self) {} }\n\n\
         pub fn other() -> u8 { 1 }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let affected = context["changes"][0]["affected_symbols"]
        .as_array()
        .unwrap();
    for owner in ["Foo", "Bar", "Holder"] {
        assert!(
            !affected.iter().any(|symbol| symbol["name"] == owner),
            "{owner}: {context}"
        );
    }
    for method in ["fresh", "bar_method", "required", "generic_method"] {
        assert!(
            affected
                .iter()
                .any(|symbol| { symbol["name"] == method && symbol["change_type"] == "added" }),
            "{method}: {context}"
        );
    }

    // impl 外に削除行を含む変更があっても、メソッド追加の owner は出さない。
    let with_methods = std::fs::read_to_string(repo.root().join("lib.rs")).unwrap();
    let with_other_change =
        with_methods.replace("pub fn other() -> u8 { 1 }", "pub fn other() -> u8 { 2 }");
    repo.write("lib.rs", &with_other_change);
    let context = repo.run_json("context", &["--git"]);
    let affected = context["changes"][0]["affected_symbols"]
        .as_array()
        .unwrap();
    assert!(
        !affected.iter().any(|symbol| symbol["name"] == "Foo"),
        "{context}"
    );
    assert!(
        affected
            .iter()
            .any(|symbol| { symbol["name"] == "other" && symbol["change_type"] == "modified" }),
        "{context}"
    );

    // 型宣言自体を変えた場合は、その影響を残す。
    repo.write(
        "lib.rs",
        with_other_change.replacen("pub struct Foo;", "pub struct Foo(pub u8);", 1),
    );
    let context = repo.run_json("context", &["--git"]);
    let affected = context["changes"][0]["affected_symbols"]
        .as_array()
        .unwrap();
    assert!(
        affected.iter().any(|symbol| {
            symbol["name"] == "Foo"
                && symbol["kind"] == "struct"
                && symbol["change_type"] == "modified"
        }),
        "{context}"
    );
    assert!(
        !affected
            .iter()
            .any(|symbol| { symbol["name"] == "Foo" && symbol["kind"] == "type" }),
        "{context}"
    );
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caller| caller["path"] == "consumer.rs"),
        "{context}"
    );
}

#[test]
fn impact_rust_impl_header_and_method_removal_remain_visible_as_impl() {
    for (before, after) in [
        (
            "pub struct Foo;\nimpl Foo {\n    pub fn first(&self) {}\n}\n",
            "pub struct Foo;\nimpl Foo where Foo: Send {\n    pub fn first(&self) {}\n}\n",
        ),
        (
            "pub struct Foo;\nimpl Foo {\n    pub fn first(&self) {}\n    pub fn second(&self) {}\n}\n",
            "pub struct Foo;\nimpl Foo {\n    pub fn second(&self) {}\n}\n",
        ),
        (
            "pub struct Foo;\nimpl Foo { pub fn first(&self) {} }\n",
            "pub struct Foo;\nimpl Foo { pub fn first(&self) {} pub fn second(&self) {} }\n",
        ),
        (
            "pub struct Foo;\nimpl Foo {\n    pub fn first(&self) {}\n}\n",
            "pub struct Foo;\nimpl Foo {\n    #[cfg(feature = \"special\")]\n    pub fn first(&self) {}\n}\n",
        ),
        (
            "pub struct Foo;\nimpl Foo {\n    /// Doc A\n    pub fn first(&self) {}\n    /// Doc B\n    pub fn second(&self) {}\n}\n",
            "pub struct Foo;\nimpl Foo {\n    /// Doc A\n    /// Doc B\n    pub fn second(&self) {}\n}\n",
        ),
        (
            "pub struct Foo;\nimpl Foo {\n    pub fn first(&self, value: u8) {}\n}\n",
            "pub struct Foo;\nimpl Foo {\n    pub fn first(&self, value: u16) {}\n}\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.write("lib.rs", before);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("lib.rs", after);
        let context = repo.run_json("context", &["--git"]);
        let affected = context["changes"][0]["affected_symbols"]
            .as_array()
            .unwrap();
        assert!(
            affected.iter().any(|symbol| {
                symbol["name"] == "Foo"
                    && symbol["kind"] == "impl"
                    && symbol["change_type"] == "modified"
            }),
            "{before} -> {after}: {context}"
        );
        assert!(
            !affected
                .iter()
                .any(|symbol| { symbol["name"] == "Foo" && symbol["kind"] == "type" }),
            "{before} -> {after}: {context}"
        );
    }
}

#[test]
fn impact_rust_impl_keeps_owner_when_removed_method_merges_into_neighbor() {
    let repo = TestRepo::new();
    repo.write(
        "lib.rs",
        "pub struct Foo;\nfn x() {}\nfn y() {}\nimpl Foo {\n    pub fn b(&self) {\n        x();\n    }\n    pub fn a(&self) {\n        y();\n    }\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "lib.rs",
        "pub struct Foo;\nfn x() {}\nfn y() {}\nimpl Foo {\n    pub fn b(&self) {\n        x();\n        y();\n    }\n}\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let affected = context["changes"][0]["affected_symbols"]
        .as_array()
        .unwrap();
    assert!(
        affected.iter().any(|symbol| {
            symbol["name"] == "Foo"
                && symbol["kind"] == "impl"
                && symbol["change_type"] == "modified"
        }),
        "{context}"
    );
    assert!(
        affected.iter().any(|symbol| symbol["name"] == "b"),
        "{context}"
    );
}

#[test]
fn impact_rust_new_marker_impl_keeps_its_owner() {
    let repo = TestRepo::new();
    repo.write("lib.rs", "pub struct Foo;\npub unsafe trait Marker {}\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "lib.rs",
        "pub struct Foo;\npub unsafe trait Marker {}\nunsafe impl Marker for Foo {}\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let affected = context["changes"][0]["affected_symbols"]
        .as_array()
        .unwrap();
    assert!(
        affected.iter().any(|symbol| {
            symbol["name"] == "Foo" && symbol["kind"] == "impl" && symbol["change_type"] == "added"
        }),
        "{context}"
    );
}

/// `impact --git` を実行し、(exit code, stderr) を返す。
fn run_impact_git(repo: &TestRepo) -> (Option<i32>, String) {
    run_impact_git_with_args(repo, &[])
}

fn run_impact_git_with_args(repo: &TestRepo, extra_args: &[&str]) -> (Option<i32>, String) {
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .args(extra_args)
        .output()
        .expect("failed to run astro-sight");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// review と同じ互換分類を impact に適用する。追加した末尾 default 引数を省略する
/// 既存 call はそのまま有効なので、作業ツリー / index のどちらでも blocking にしない。
#[test]
fn impact_allows_unchanged_calls_after_compatible_defaulted_parameter() {
    for staged in [false, true] {
        let repo = TestRepo::new();
        repo.write(
            "library.ts",
            "export function formatLabel(value: string) { return value.trim(); }\n",
        );
        repo.write(
            "consumer.ts",
            "import { formatLabel } from './library';\nconsole.log(formatLabel(' Ready '));\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "library.ts",
            "export function formatLabel(value: string, options: { verbose?: boolean } = {}) {\n  return value.trim();\n}\n",
        );
        if staged {
            repo.stage_all();
        }

        let review = if staged {
            repo.run_json("review", &["--git", "--staged"])
        } else {
            repo.run_json("review", &["--git"])
        };
        assert!(
            review["api_changes"]["compatible_modified"]
                .as_array()
                .is_some_and(|changes| {
                    changes.iter().any(|change| {
                        change["file"] == "library.ts"
                            && change["name"] == "formatLabel"
                            && change["reason"] == "trailing_optional_params"
                    })
                }),
            "staged={staged}: {review}"
        );

        let extra_args = if staged { &["--staged"][..] } else { &[] };
        let (code, stderr) = run_impact_git_with_args(&repo, extra_args);
        assert_eq!(code, Some(0), "staged={staged}: {stderr}");
        assert!(stderr.is_empty(), "staged={staged}: {stderr}");
    }
}

/// 互換と判定されたシンボルだけを抑制し、同じ変更ファイルの必須引数追加は blocking に残す。
#[test]
fn impact_keeps_incompatible_symbols_blocking_beside_compatible_changes() {
    let repo = TestRepo::new();
    repo.write(
        "library.ts",
        "export function formatLabel(value: string) { return value.trim(); }\nexport function requireOption(value: string) { return value; }\n",
    );
    repo.write(
        "consumer.ts",
        "import { formatLabel, requireOption } from './library';\nconsole.log(formatLabel(' Ready '));\nconsole.log(requireOption(' Ready '));\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "library.ts",
        "export function formatLabel(value: string, options: { verbose?: boolean } = {}) { return value.trim(); }\nexport function requireOption(value: string, required: boolean) { return value; }\n",
    );

    let (code, stderr) = run_impact_git(&repo);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("consumer.ts:3 [requireOption]"), "{stderr}");
    assert!(!stderr.contains("consumer.ts:2 [formatLabel]"), "{stderr}");
}

/// 自由関数の互換証拠で、同名メソッドの必須引数追加を隠さない。
#[test]
fn impact_keeps_same_named_incompatible_declarations_blocking() {
    for (extension, old, new, consumer) in [
        (
            "ts",
            "export function render(value: string) { return value; }\nexport class Label {\n  render(value: string) { return value; }\n}\n",
            "export function render(value: string, options = {}) { return value; }\nexport class Label {\n  render(value: string, required: boolean) { return value; }\n}\n",
            "import {Label, render} from './library';\nconsole.log(render('x'));\nconst label = new Label();\nconsole.log(label.render('x'));\n",
        ),
        (
            "ts",
            "export function render(value: string) { return value; }\nclass Hidden {\n  render(value: string) { return value; }\n}\nexport function makeLabel() { return new Hidden(); }\n",
            "export function render(value: string, options = {}) { return value; }\nclass Hidden {\n  render(value: string, required: boolean) { return value; }\n}\nexport function makeLabel() { return new Hidden(); }\n",
            "import {makeLabel, render} from './library';\nconsole.log(render('x'));\nconst label = makeLabel();\nconsole.log(label.render('x'));\n",
        ),
        (
            "py",
            "def render(value):\n    return value\n\nclass Label:\n    def render(self, value):\n        return value\n",
            "def render(value, options=None):\n    return value\n\nclass Label:\n    def render(self, value, required):\n        return value\n",
            "from library import Label, render\nprint(render('x'))\nlabel = Label()\nprint(label.render('x'))\n",
        ),
        (
            "py",
            "def render(value):\n    return value\n\nclass Label:\n    def render(self, value):\n        return value\n",
            "def render(value, options=None):\n    return value\n\nclass Label:\n    pass\n",
            "from library import Label, render\nprint(render('x'))\nlabel = Label()\nprint(label.render('x'))\n",
        ),
        (
            "py",
            "def render(value):\n    return value\n\nclass Label:\n    def render(self, value):\n        return value\n",
            "def render(value, options=None):\n    return value\n\nclass Label:\n    def render(self, value):\n        return value\n",
            "from library import Label, render\nprint(render('x'))\nlabel = Label()\nprint(label.render('x'))\n",
        ),
    ] {
        for staged in [false, true] {
            let repo = TestRepo::new();
            repo.write(format!("library.{extension}"), old);
            repo.write(format!("consumer.{extension}"), consumer);
            repo.init_git();
            repo.commit_all("initial");
            repo.write(format!("library.{extension}"), new);
            if staged {
                repo.stage_all();
            }
            let args = if staged { &["--staged"][..] } else { &[] };
            let (code, stderr) = run_impact_git_with_args(&repo, args);
            assert_eq!(code, Some(1), "{extension}, staged={staged}: {stderr}");
            assert!(
                stderr.contains(&format!("consumer.{extension}:4 [render]")),
                "{extension}, staged={staged}: {stderr}"
            );
        }
    }
}

/// 1 行の caller が互換・非互換の両方を参照しても、非互換な名前を残す。
#[test]
fn impact_keeps_mixed_symbols_on_one_caller_line() {
    let repo = TestRepo::new();
    repo.write("library.ts", "export function format(value: string) { return value; }\nexport function render(value: string) { return value; }\n");
    repo.write(
        "consumer.ts",
        "import {format, render} from './library';\nconsole.log(format('x'), render('x'));\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("library.ts", "export function format(value: string, options = {}) { return value; }\nexport function render(value: string, required: boolean) { return value; }\n");
    let (code, stderr) = run_impact_git(&repo);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("consumer.ts:2 [render]"), "{stderr}");
    assert!(!stderr.contains("format"), "{stderr}");
}

/// 別ファイルの同名宣言を削除しても、互換な宣言の caller に混ざった参照を隠さない。
#[test]
fn impact_keeps_other_files_removed_declarations_blocking() {
    for (removed, call) in [
        (
            "export function render(value: string) { return value; }\n",
            "broken.render('x')",
        ),
        (
            "function render(value: string) { return value; }\nexport const label = {render};\n",
            "broken.label.render('x')",
        ),
    ] {
        let repo = TestRepo::new();
        repo.write(
            "safe.ts",
            "export function render(value: string) { return value; }\n",
        );
        repo.write("broken.ts", removed);
        repo.write(
            "consumer.ts",
            format!("import * as safe from './safe';\nimport * as broken from './broken';\nconsole.log(safe.render('x'));\nconsole.log({call});\n"),
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "safe.ts",
            "export function render(value: string, options = {}) { return value; }\n",
        );
        repo.write("broken.ts", "export {};\n");
        let (code, stderr) = run_impact_git(&repo);
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("consumer.ts:4 [render]"), "{stderr}");
    }
}

/// 変更したソースを解析できなければ証明を保留し、通常の文書変更は許す。
#[test]
fn impact_keeps_compatibility_unverified_for_unparsed_changes() {
    for (path, source, expected) in [
        ("other.ts", "export function broken( {\n", 1),
        ("other.vue", "<script>const label = 'x';</script>\n", 1),
        ("README.md", "Updated documentation.\n", 0),
    ] {
        let repo = TestRepo::new();
        repo.write(
            "library.ts",
            "export function render(value: string) { return value; }\n",
        );
        repo.write(
            "consumer.ts",
            "import {render} from './library';\nconsole.log(render('x'));\n",
        );
        repo.write(path, "/* Initial content. */\n");
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "library.ts",
            "export function render(value: string, options = {}) { return value; }\n",
        );
        repo.write(path, source);
        let (code, stderr) = run_impact_git(&repo);
        assert_eq!(code, Some(expected), "{path}: {stderr}");
        if expected == 1 {
            assert!(
                stderr.contains("consumer.ts:2 [render]"),
                "{path}: {stderr}"
            );
        }
    }
}

/// API の所有型付き名と caller の素のメソッド名を安全に対応付ける。
#[test]
fn impact_allows_compatible_python_method_calls() {
    for staged in [false, true] {
        let repo = TestRepo::new();
        repo.write(
            "library.py",
            "class Label:\n    def render(self, value):\n        return value\n",
        );
        repo.write(
            "consumer.py",
            "from library import Label\nlabel = Label()\nprint(label.render('x'))\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "library.py",
            "class Label:\n    def render(self, value, options=None):\n        return value\n",
        );
        if staged {
            repo.stage_all();
        }
        let args = if staged { &["--staged"][..] } else { &[] };
        let review = repo.run_json("review", &[&["--git"][..], args].concat());
        assert!(
            review["api_changes"]["compatible_modified"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["name"] == "Label.render"),
            "{review}"
        );
        let (code, stderr) = run_impact_git_with_args(&repo, args);
        assert_eq!(code, Some(0), "staged={staged}: {stderr}");
        assert!(stderr.is_empty(), "staged={staged}: {stderr}");
    }
}

/// 同名宣言が複数あっても、すべての所有元の互換性を証明できれば除外する。
#[test]
fn impact_allows_same_named_declarations_when_all_are_compatible() {
    let repo = TestRepo::new();
    repo.write(
        "library.py",
        "def render(value):\n    return value\n\nclass Label:\n    def render(self, value):\n        return value\n",
    );
    repo.write(
        "consumer.py",
        "from library import Label, render\nprint(render('x'))\nlabel = Label()\nprint(label.render('x'))\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "library.py",
        "def render(value, options=None):\n    return value\n\nclass Label:\n    def render(self, value, options=None):\n        return value\n",
    );
    let (code, stderr) = run_impact_git(&repo);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

/// 関数シグネチャ差分がなくても、型への optional メンバー追加は互換判定する。
#[test]
fn impact_allows_compatible_type_members_without_signature_changes() {
    for staged in [false, true] {
        let repo = TestRepo::new();
        repo.write("library.ts", "export interface Config { name: string; }\n");
        repo.write(
            "consumer.ts",
            "import {Config} from './library';\nexport const config: Config = {name: 'x'};\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "library.ts",
            "export interface Config { name: string; extra?: boolean; }\n",
        );
        if staged {
            repo.stage_all();
        }
        let args = if staged { &["--staged"][..] } else { &[] };
        let review = repo.run_json("review", &[&["--git"][..], args].concat());
        let change = &review["impact"]["changes"][0];
        assert!(change.get("signature_changes").is_none(), "{review}");
        assert_eq!(
            review["api_changes"]["compatible_modified"][0]["reason"], "optional_type_members",
            "{review}"
        );
        let (code, stderr) = run_impact_git_with_args(&repo, args);
        assert_eq!(code, Some(0), "staged={staged}: {stderr}");
        assert!(stderr.is_empty(), "staged={staged}: {stderr}");
    }
}

/// index と作業ツリーのどちらが互換でも、両者が異なれば互換証拠を使わない。
#[test]
fn impact_keeps_staged_compatibility_unverified_when_worktree_differs() {
    let compatible = "export function render(value: string, options = {}) { return value; }\n";
    let incompatible =
        "export function render(value: string, required: boolean) { return value; }\n";
    for (index, worktree) in [(compatible, incompatible), (incompatible, compatible)] {
        let repo = TestRepo::new();
        repo.write(
            "library.ts",
            "export function render(value: string) { return value; }\n",
        );
        repo.write(
            "consumer.ts",
            "import {render} from './library';\nconsole.log(render('x'));\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("library.ts", index);
        repo.stage_all();
        repo.write("library.ts", worktree);
        let (code, stderr) = run_impact_git_with_args(&repo, &["--staged"]);
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("consumer.ts:2 [render]"), "{stderr}");
    }
}

/// 言語ごとに、変更する公開関数を呼ぶファイルを置く。
/// 戻り値は (変更するファイル, 報告されるべき呼び出し元 `path:line`)。
fn write_public_function_caller(repo: &TestRepo, lang: &str) -> (&'static str, &'static str) {
    match lang {
        "ts" => {
            repo.create_dir_all("src");
            repo.write(
                "src/use.ts",
                "import { format } from './a';\n\nexport function caller(): string {\n  return format(1);\n}\n",
            );
            ("src/a.ts", "src/use.ts:4")
        }
        "py" => {
            repo.create_dir_all("pkg");
            repo.write("pkg/__init__.py", "");
            repo.write(
                "pkg/use.py",
                "from pkg.a import render\n\n\ndef caller():\n    return render(1)\n",
            );
            ("pkg/a.py", "pkg/use.py:5")
        }
        "rs" => {
            repo.create_dir_all("src");
            repo.write(
                "Cargo.toml",
                "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            );
            repo.write("src/lib.rs", "pub mod a;\npub mod user;\n");
            repo.write(
                "src/user.rs",
                "use crate::a::helper;\n\npub fn caller() -> u32 {\n    helper(1)\n}\n",
            );
            ("src/a.rs", "src/user.rs:4")
        }
        other => unreachable!("unknown fixture language: {other}"),
    }
}

/// 変更した公開関数の直前に同名のローカル const・入れ子の関数・非公開メソッドがあっても、
/// affected を作った元のシンボル自身でテスト / ローカル / 公開を判定する。
/// 旧実装は「hunk (context 行を含む) に重なる最初の同名シンボル」を名前で引き直して判定し、
/// 手前の同名シンボルをローカル・非公開と見て公開関数の呼び出し元を落としていた (exit 0)。
#[test]
fn impact_judges_each_affected_symbol_by_its_own_declaration() {
    // 言語ごとの公開関数 (変更前, 変更後)。変更は必須引数の追加 (呼び出し側が壊れる)。
    let public_function = |lang: &str| match lang {
        "ts" => (
            "export function format(value: number): string {\n  return String(value);\n}\n",
            "export function format(value: number, digits: number): string {\n  return value.toFixed(digits);\n}\n",
        ),
        "py" => (
            "def render(value):\n    return str(value)\n",
            "def render(value, digits):\n    return f\"{value:.{digits}f}\"\n",
        ),
        _ => (
            "pub fn helper(value: u32) -> u32 {\n    value\n}\n",
            "pub fn helper(value: u32, extra: u32) -> u32 {\n    value + extra\n}\n",
        ),
    };
    // (ラベル, 言語, 公開関数の手前に置く同名の宣言, 対照として名前だけ変えた宣言)
    let cases = [
        (
            "TS local const",
            "ts",
            "export function App() {\n  const format = (v: number) => v.toFixed(2);\n  return format(1);\n}\n",
            "export function App() {\n  const fmt = (v: number) => v.toFixed(2);\n  return fmt(1);\n}\n",
        ),
        (
            "TS private class method",
            "ts",
            "class Formatter {\n  format(v: number): string { return String(v); }\n}\n",
            "class Formatter {\n  render(v: number): string { return String(v); }\n}\n",
        ),
        (
            "Python nested def",
            "py",
            "def app():\n    def render(v):\n        return v\n    return render(1)\n",
            "def app():\n    def helper(v):\n        return v\n    return helper(1)\n",
        ),
        (
            "Rust nested fn",
            "rs",
            "pub fn app() -> u32 {\n    fn helper(v: u32) -> u32 { v }\n    helper(1)\n}\n",
            "pub fn app() -> u32 {\n    fn local_helper(v: u32) -> u32 { v }\n    local_helper(1)\n}\n",
        ),
    ];
    for (label, lang, same_name, other_name) in cases {
        let (before, after) = public_function(lang);
        for (variant, head) in [("同名", same_name), ("別名 (対照)", other_name)] {
            let repo = TestRepo::new();
            let (changed_path, caller) = write_public_function_caller(&repo, lang);
            repo.write(changed_path, format!("{head}{before}"));
            repo.init_git();
            repo.commit_all("initial");
            repo.write(changed_path, format!("{head}{after}"));
            let (code, stderr) = run_impact_git(&repo);
            assert_eq!(
                code,
                Some(1),
                "{label} / {variant}: 公開関数の必須引数追加は blocking にすべき: {stderr}"
            );
            assert!(
                stderr.contains(&format!("→ {caller}")),
                "{label} / {variant}: 呼び出し元 {caller} を報告すべき: {stderr}"
            );
        }
    }
}

/// 親型の解決と公開判定も、変更した宣言自身で行う。
/// - 公開クラスの同名メソッドと公開関数を両方変えた: 関数の呼び出し元を、メソッドの親型
///   (`Formatter`) が出てくるファイルに絞らない (旧実装は手前のメソッドの親型を名前に紐づけ、
///   `Formatter` を書いていない呼び出し元を落としていた)。
/// - 非公開クラスのメソッドだけを変えた: 手前にある未変更の公開関数で公開判定しない
///   (旧実装は同名の公開関数の呼び出し元を blocking にしていた。取り違えの逆方向)。
/// - 対照: 公開関数だけを変えれば、従来どおり呼び出し元を報告する。
#[test]
fn impact_parent_type_and_export_check_follow_the_changed_declaration() {
    let function = |params: &str| {
        format!("export function format({params}): string {{\n  return String(value);\n}}\n")
    };
    let method = |params: &str| {
        format!("class Formatter {{\n  format({params}): string {{ return String(v); }}\n}}\n")
    };
    let exported_method = |params: &str| format!("export {}", method(params));
    // (ラベル, 変更前, 変更後, 呼び出し元を報告すべきか)
    let cases = [
        (
            "公開クラスの同名メソッドと公開関数を両方変更",
            format!(
                "{}{}",
                exported_method("v: number"),
                function("value: number")
            ),
            format!(
                "{}{}",
                exported_method("v: number, d: number"),
                function("value: number, digits: number")
            ),
            true,
        ),
        (
            "非公開クラスのメソッドだけを変更",
            format!("{}{}", function("value: number"), method("v: number")),
            format!(
                "{}{}",
                function("value: number"),
                method("v: number, d: number")
            ),
            false,
        ),
        (
            "対照: 公開関数だけを変更",
            format!("{}{}", function("value: number"), method("v: number")),
            format!(
                "{}{}",
                function("value: number, digits: number"),
                method("v: number")
            ),
            true,
        ),
    ];
    for (label, before, after, expect_caller) in cases {
        let repo = TestRepo::new();
        let (changed_path, caller) = write_public_function_caller(&repo, "ts");
        repo.write(changed_path, &before);
        repo.init_git();
        repo.commit_all("initial");
        repo.write(changed_path, &after);
        let (code, stderr) = run_impact_git(&repo);
        if expect_caller {
            assert_eq!(code, Some(1), "{label}: {stderr}");
            assert!(
                stderr.contains(&format!("→ {caller}")),
                "{label}: 呼び出し元 {caller} を報告すべき: {stderr}"
            );
        } else {
            assert_eq!(code, Some(0), "{label}: 呼び出し元は無いはず: {stderr}");
            assert!(!stderr.contains(caller), "{label}: {stderr}");
        }
    }
}

/// 移動しただけの 1 行 (`export const run = async () => foo(1);`) が、同じ diff の
/// `foo` のシグネチャ変更を隠さない。行ベースの検出は名前を含むシグネチャらしい最初の
/// `-` 行と `+` 行を比べるので、両側で先に見つかる囮行同士が一致して「変更なし」になり、
/// 旧実装はその結論を AST の比較より優先していた (exit 0、実際は TS2322 / TS2554)。
#[test]
fn impact_moved_decoy_line_does_not_hide_signature_change() {
    let decoy = "export const run = async () => foo(1);\n";
    let bar = "export function bar(): number {\n  return 2;\n}\n";
    let old_foo = "export function foo(x: number): number {\n  return x;\n}\n";
    for (label, new_foo) in [
        (
            "戻り値型の変更",
            "export function foo(x: number): string {\n  return String(x);\n}\n",
        ),
        (
            "必須引数の追加",
            "export function foo(x: number, y: number): number {\n  return x + y;\n}\n",
        ),
    ] {
        // 囮行を foo の直前へ動かす (修正前に見逃した形) / 動かさない (対照)。
        for (variant, after) in [
            ("囮行を移動", format!("{bar}{decoy}{new_foo}")),
            ("囮行はそのまま", format!("{decoy}{bar}{new_foo}")),
        ] {
            let repo = TestRepo::new();
            repo.create_dir_all("src");
            repo.write("src/a.ts", format!("{decoy}{bar}{old_foo}"));
            repo.write(
                "src/use.ts",
                "import { foo } from './a';\n\nexport function caller(): number {\n  const n: number = foo(1);\n  return n;\n}\n",
            );
            repo.init_git();
            repo.commit_all("initial");
            repo.write("src/a.ts", &after);
            let (code, stderr) = run_impact_git(&repo);
            assert_eq!(code, Some(1), "{label} / {variant}: {stderr}");
            assert!(
                stderr.contains("→ src/use.ts:4"),
                "{label} / {variant}: {stderr}"
            );
        }
    }
}

/// `= require(` を含む文を丸ごと import とみなさない。同じ文の実際の呼び出し
/// (`require('./a').foo(1)`) を import 行 (情報提供) として扱い、旧実装は exit 0 にしていた。
/// 対照: CommonJS の束縛 (`const foo = require('./a').foo`) の行は従来どおり情報提供で、
/// その束縛を呼ぶ行が blocking になる。
#[test]
fn impact_require_call_on_import_line_is_a_blocking_caller() {
    // (呼び出し側, blocking な行 (0 始まり), 情報提供の行)
    let cases: [(&str, &[u64], &[u64]); 5] = [
        (
            "const value = require('./a').foo(1);\nmodule.exports = { value };\n",
            &[0],
            &[],
        ),
        (
            "const { foo } = require('./a'), value = foo(1);\nmodule.exports = { value };\n",
            &[0],
            &[],
        ),
        ("require('./a').foo(1);\n", &[0], &[]),
        (
            "const foo = require('./a').foo;\nconst value = foo(1);\nmodule.exports = { value };\n",
            &[1],
            &[0],
        ),
        (
            "const { foo } = require('./a');\nconst value = foo(1);\nmodule.exports = { value };\n",
            &[1],
            &[],
        ),
    ];
    for (consumer, blocking, informational) in cases {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write("src/a.js", "export function foo(x) {\n  return x;\n}\n");
        repo.write("src/use.js", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/a.js",
            "export function foo(x, y) {\n  if (y === undefined) throw new Error(\"y required\");\n  return x + y;\n}\n",
        );
        let (code, stderr) = run_impact_git(&repo);
        assert_eq!(code, Some(1), "{consumer}: {stderr}");
        let context = repo.run_json("context", &["--git"]);
        let lines = |key: &str| -> Vec<u64> {
            context["changes"][0][key]
                .as_array()
                .map(|callers| {
                    callers
                        .iter()
                        .filter(|caller| caller["path"] == "src/use.js")
                        .filter_map(|caller| caller["line"].as_u64())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(lines("impacted_callers"), blocking, "{consumer}: {context}");
        assert_eq!(
            lines("informational_callers"),
            informational,
            "{consumer}: {context}"
        );
    }
}

/// 参照走査の準備 (対象ファイルの列挙) に失敗したら、空の結果ではなくエラー (exit 1) にする。
/// 旧実装は失敗を握りつぶして cross-file の呼び出し元を 0 件にしていたため、読めない
/// ディレクトリが 1 つあるだけで `impact --hook` が exit 0 で未解決の呼び出し元を通していた
/// (同じ状態で `refs` / `dead-code` は IO_ERROR)。`context` は書きかけの JSON の後ろに
/// エラーを付けず、エラーの JSON だけを返す。
/// 対照: 同じリポジトリでディレクトリを読めるように戻せば、従来どおり呼び出し元を報告する。
#[cfg(unix)]
#[test]
fn impact_scan_failure_is_an_error_not_an_empty_result() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "src/a.ts",
        "export function foo(x: number): number {\n  return x;\n}\n",
    );
    repo.write(
        "src/use.ts",
        "import { foo } from './a';\nexport const v = foo(1);\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/a.ts",
        "export function foo(x: number, y: number): number {\n  return x + y;\n}\n",
    );
    repo.create_dir_all("locked");
    let set_mode = |mode: u32| {
        std::fs::set_permissions(repo.path("locked"), std::fs::Permissions::from_mode(mode))
            .expect("chmod");
    };
    set_mode(0o000);
    // 特権で実行すると権限を外しても読めてしまい、失敗を再現できない。
    if std::fs::read_dir(repo.path("locked")).is_ok() {
        set_mode(0o755);
        eprintln!("skip: 読めないディレクトリを作れない (特権で実行している)");
        return;
    }

    let run = |args: &[&str]| {
        let output = cargo_bin()
            .args(args)
            .arg("--dir")
            .arg(repo.root())
            .output()
            .expect("failed to run astro-sight");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    };
    for args in [
        &["impact", "--git", "--hook"][..],
        &["impact", "--git"][..],
        &["context", "--git"][..],
        &["review", "--git", "--hook"][..],
    ] {
        let (code, stdout) = run(args);
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(stdout.trim());
        set_mode(0o755);
        assert_eq!(code, Some(1), "{args:?}: {stdout}");
        let json =
            parsed.unwrap_or_else(|e| panic!("{args:?}: 出力が JSON でない ({e}): {stdout}"));
        assert_eq!(json["error"]["code"], "IO_ERROR", "{args:?}: {json}");
        assert!(
            json.get("changes").is_none(),
            "{args:?}: 書きかけの結果を出さない: {json}"
        );
        set_mode(0o000);
    }
    set_mode(0o755);

    // 対照: 読めるディレクトリなら従来どおり呼び出し元を報告する。
    let (code, stderr) = run_impact_git(&repo);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("→ src/use.ts:2"), "{stderr}");
}

/// 波括弧の struct を別モジュールへ移して旧パスを `pub use` で再公開するとき、移動先で
/// フィールドを足した変更は「純粋な移動」ではない (`Shape { w: 1.0 }` が E0063 で壊れる)。
/// 旧実装は宣言の 1 行目 (`pub struct Shape {`) だけを比べて informational に降格し、
/// `impact --hook` / `review --hook` がどちらも exit 0 だった。
/// 対照: 同じ struct をそのまま移した場合は従来どおり informational (exit 0)。
/// `--staged` では移動元・移動先の両方を index から読んで照合する (作業ツリーに逆の内容を
/// 置いても index の内容で判定する)。
#[test]
fn reexport_move_of_brace_struct_compares_fields() {
    let shape = "pub struct Shape {\n    pub w: f32,\n}\n";
    let widened = "pub struct Shape {\n    pub w: f32,\n    pub h: f32,\n}\n";
    for (label, moved_shape, unstaged_shape, blocking) in [
        ("フィールドを追加して移動", widened, shape, true),
        ("そのまま移動 (対照)", shape, widened, false),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src/game");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod game;\n");
        repo.write(
            "src/game/mod.rs",
            "pub mod background;\npub mod consumer;\n",
        );
        let background = "pub const BASE_Y: f32 = -70.0;\npub const BAND_H: f32 = 30.0;\n\npub fn draw() -> f32 {\n    BASE_Y + BAND_H\n}\n";
        repo.write("src/game/background.rs", background);
        let make = "\npub fn make() -> Shape {\n    Shape { w: 1.0 }\n}\n";
        repo.write("src/game/consumer.rs", format!("{shape}{make}"));
        repo.init_git();
        repo.commit_all("initial");
        // 移動先: BAND_H の値変更と同じ hunk に Shape を足す。移動元: 定義を再輸出に置き換える。
        repo.write(
            "src/game/background.rs",
            background.replace(
                "pub const BAND_H: f32 = 30.0;\n",
                &format!("pub const BAND_H: f32 = 40.0;\n{moved_shape}"),
            ),
        );
        repo.write(
            "src/game/consumer.rs",
            format!("pub use super::background::Shape;\n{make}"),
        );

        for subcommand in ["impact", "review"] {
            let output = cargo_bin()
                .args([
                    subcommand,
                    "--dir",
                    repo.root().to_str().unwrap(),
                    "--git",
                    "--hook",
                ])
                .output()
                .expect("failed to run astro-sight");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                !output.status.success(),
                blocking,
                "{label} / {subcommand}: {stderr}"
            );
        }
        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/game/background.rs")
            .unwrap_or_else(|| panic!("{label}: {context}"));
        let key = if blocking {
            "impacted_callers"
        } else {
            "informational_callers"
        };
        assert!(
            change[key].as_array().is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/game/consumer.rs")),
            "{label}: consumer.rs の参照は {key} に出るべき: {context}"
        );

        repo.stage_all();
        repo.write(
            "src/game/background.rs",
            background.replace(
                "pub const BAND_H: f32 = 30.0;\n",
                &format!("pub const BAND_H: f32 = 40.0;\n{unstaged_shape}"),
            ),
        );
        for subcommand in ["impact", "review"] {
            let output = cargo_bin()
                .args([
                    subcommand,
                    "--dir",
                    repo.root().to_str().unwrap(),
                    "--git",
                    "--staged",
                    "--hook",
                ])
                .output()
                .expect("failed to run astro-sight");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                !output.status.success(),
                blocking,
                "{label} / {subcommand} --staged: {stderr}"
            );
        }
    }
}

/// private な inherent メソッドを属性の文字列 (`#[serde(skip_serializing_if = "...")]`) だけで
/// 参照する子モジュールを、呼び出し元から捨てない。文字列由来の参照は AST ノードを持たず、
/// 構文でメソッドを指さないと証明できないので、部分木を証明できなければ低確信度に残す
/// (同じ構成で直接呼んだ場合と同じ扱い。serde の属性があると private scope の証明は無効になる)。
/// 旧実装はノードの無い参照を「メソッドの構文ではない」として無条件に捨てていた。
/// 対照: 同じメソッドを `pub` にすれば従来どおり blocking。
#[test]
fn impact_rust_private_method_referenced_from_attribute_string_is_kept() {
    for (visibility, expected_key) in [("", "low_confidence_callers"), ("pub ", "impacted_callers")]
    {
        let repo = TestRepo::new();
        repo.create_dir_all("src/config");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod config;\n");
        let config = |parameter: &str| {
            format!(
                "mod file;\npub struct Limits;\nimpl Limits {{\n    {visibility}fn is_unset(v: &{parameter}) -> bool {{\n        *v == 0\n    }}\n}}\n"
            )
        };
        repo.write("src/config.rs", config("u32"));
        repo.write(
            "src/config/file.rs",
            "#[derive(serde::Serialize)]\npub struct FileConfig {\n    #[serde(skip_serializing_if = \"super::Limits::is_unset\")]\n    pub max: u32,\n}\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/config.rs", config("u64"));

        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/config.rs")
            .unwrap_or_else(|| panic!("visibility={visibility:?}: {context}"));
        let callers = |key: &str| -> Vec<(String, u64)> {
            change[key]
                .as_array()
                .map(|callers| {
                    callers
                        .iter()
                        .map(|caller| {
                            (
                                caller["path"].as_str().unwrap_or_default().to_string(),
                                caller["line"].as_u64().unwrap_or_default(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            callers(expected_key),
            vec![("src/config/file.rs".to_string(), 2)],
            "visibility={visibility:?}: {context}"
        );
    }
}

/// 別名 import (`use crate::a::check as is_zero;`) の別名を属性の文字列
/// (`#[serde(skip_serializing_if = "is_zero")]`) だけで使う場合も、元名の import 行を
/// blocking に残す。別名の使用は元名の参照として検索に出ないので、import 行を情報提供へ
/// 落とすと blocking がどこにも出ない。旧実装は別名の使用を識別子ノードでしか探さず、
/// 引数型の変更が hook を素通りしていた。
/// 対照: 呼び出しで使えば従来どおり blocking、使わない場合・属性の外の文字列・属性内でも
/// 識別子として現れない場合 (`is_zero_ext`) は従来どおり情報提供。
#[test]
fn impact_rust_alias_used_only_in_attribute_string_keeps_import_blocking() {
    let attribute_use = |name: &str| {
        format!(
            "use crate::a::check as is_zero;\n#[derive(serde::Serialize)]\npub struct Payload {{\n    #[serde(skip_serializing_if = \"{name}\")]\n    pub v: u32,\n}}\n"
        )
    };
    let cases = [
        ("attribute", attribute_use("is_zero"), "impacted_callers"),
        (
            "call",
            "use crate::a::check as is_zero;\npub fn empty(v: &u32) -> bool {\n    is_zero(v)\n}\n"
                .to_string(),
            "impacted_callers",
        ),
        (
            "unused",
            "use crate::a::check as is_zero;\npub struct Payload {\n    pub v: u32,\n}\n"
                .to_string(),
            "informational_callers",
        ),
        (
            "plain string",
            "use crate::a::check as is_zero;\npub const NAME: &str = \"is_zero\";\n".to_string(),
            "informational_callers",
        ),
        (
            "other name in attribute",
            attribute_use("is_zero_ext"),
            "informational_callers",
        ),
    ];
    for (label, caller, expected_key) in cases {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod a;\npub mod c;\n");
        let check = |parameter: &str| {
            format!("pub(crate) fn check(v: &{parameter}) -> bool {{\n    *v == 0\n}}\n")
        };
        repo.write("src/a.rs", check("u32"));
        repo.write("src/c.rs", caller);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/a.rs", check("u64"));

        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/a.rs")
            .unwrap_or_else(|| panic!("{label}: {context}"));
        for key in [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers",
        ] {
            let callers = change[key]
                .as_array()
                .map(|callers| {
                    callers
                        .iter()
                        .map(|caller| {
                            (
                                caller["path"].as_str().unwrap_or_default().to_string(),
                                caller["line"].as_u64().unwrap_or_default(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let expected = if key == expected_key {
                vec![("src/c.rs".to_string(), 0)]
            } else {
                Vec::new()
            };
            assert_eq!(callers, expected, "{label} / {key}: {context}");
        }
    }
}

/// Rust の引数の束縛に付く `mut` (`mut self`・`mut x`・パターンの中の `mut`) だけの変更は、
/// 関数の型にも呼び出し方にも影響しないので、impact と review の hook を止めない。
/// 修正前は `mut self` とパターンの中の `mut` をシグネチャ変更とみなし、呼び出し元を
/// 未解決の影響 (impact --hook) と api.mod (review --hook) として報告していた。1 行の関数
/// (本体の変更と同時) でも同じ。
/// 対照: 型の `mut` (`&self` → `&mut self`、`&T` → `&mut T`、`*const` → `*mut`) は契約の
/// 変更なので、従来どおり両方の hook が止める。
#[test]
fn rust_binding_mut_only_change_does_not_block_hooks() {
    let s = "pub struct S {\n    pub v: u32,\n}\n";
    // (ラベル, 変更前, 変更後, b.rs の呼び出し, hook が止めるべきか)
    let cases = [
        (
            "mut self",
            "impl S {\n    pub fn take(self) -> u32 {\n        self.v\n    }\n}\n",
            "impl S {\n    pub fn take(mut self) -> u32 {\n        self.v += 0;\n        self.v\n    }\n}\n",
            "S { v: 1 }.take()",
            false,
        ),
        (
            "mut x",
            "pub fn add(x: u32) -> u32 {\n    x\n}\n",
            "pub fn add(mut x: u32) -> u32 {\n    x += 0;\n    x\n}\n",
            "crate::a::add(1)",
            false,
        ),
        (
            "mut in pattern",
            "pub fn sum((a, b): (u32, u32)) -> u32 {\n    a + b\n}\n",
            "pub fn sum((mut a, b): (u32, u32)) -> u32 {\n    a += 0;\n    a + b\n}\n",
            "crate::a::sum((1, 2))",
            false,
        ),
        (
            "one-line mut self",
            "impl S {\n    pub fn take(self) -> u32 { self.v }\n}\n",
            "impl S {\n    pub fn take(mut self) -> u32 { self.v += 0; self.v }\n}\n",
            "S { v: 1 }.take()",
            false,
        ),
        (
            "&mut self",
            "impl S {\n    pub fn peek(&self) -> u32 {\n        self.v\n    }\n}\n",
            "impl S {\n    pub fn peek(&mut self) -> u32 {\n        self.v\n    }\n}\n",
            "S { v: 1 }.peek()",
            true,
        ),
        (
            "&mut T",
            "pub fn add(x: &u32) -> u32 {\n    *x\n}\n",
            "pub fn add(x: &mut u32) -> u32 {\n    *x\n}\n",
            "crate::a::add(&1)",
            true,
        ),
        (
            "*mut T",
            "pub fn raw(p: *const u8) -> bool {\n    p.is_null()\n}\n",
            "pub fn raw(p: *mut u8) -> bool {\n    p.is_null()\n}\n",
            "crate::a::raw(std::ptr::null())",
            true,
        ),
    ];
    for (label, old, new, call, blocking) in cases {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod a;\npub mod b;\n");
        repo.write("src/a.rs", format!("{s}{old}"));
        repo.write(
            "src/b.rs",
            format!("use crate::a::S;\npub fn run() {{\n    let _ = {call};\n}}\n"),
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/a.rs", format!("{s}{new}"));

        let dir = repo.root().to_str().unwrap();
        let impact = cargo_bin()
            .args(["impact", "--dir", dir, "--git", "--hook"])
            .output()
            .expect("failed to run astro-sight");
        let impact_stderr = String::from_utf8_lossy(&impact.stderr);
        let review = cargo_bin()
            .args(["review", "--dir", dir, "--git", "--hook"])
            .output()
            .expect("failed to run astro-sight");
        let review_stderr = String::from_utf8_lossy(&review.stderr);
        if blocking {
            assert_eq!(impact.status.code(), Some(1), "{label}: {impact_stderr}");
            assert!(
                impact_stderr.contains("→ src/b.rs:3"),
                "{label}: {impact_stderr}"
            );
            assert_eq!(review.status.code(), Some(1), "{label}: {review_stderr}");
            let json: serde_json::Value =
                serde_json::from_str(review_stderr.trim()).expect("review hook json");
            assert_eq!(
                json["blocking_categories"],
                serde_json::json!(["impacts", "api.mod"]),
                "{label}: {review_stderr}"
            );
        } else {
            assert_eq!(impact.status.code(), Some(0), "{label}: {impact_stderr}");
            assert_eq!(review.status.code(), Some(0), "{label}: {review_stderr}");
            assert!(
                review_stderr.trim().is_empty(),
                "{label}: 束縛の mut だけの変更は報告しない: {review_stderr}"
            );
        }
    }
}
