//! 公開クラスの同じメソッドが基底に残るときは削除にしない。

use super::support::*;

const OLD: &str = "class Square:\n    side = 2.0\n\n    def area(self) -> float:\n        return self.side * self.side\n\n    def banner(self) -> str:\n        return f\"area={self.area()}\"\n";
const BASE: &str = "class Shape:\n    def area(self) -> float:\n        raise NotImplementedError\n\n    def banner(self) -> str:\n        return f\"area={self.area()}\"\n";
const OWNER: &str = "class Square(Shape):\n    side = 2.0\n\n    def area(self) -> float:\n        return self.side * self.side\n";

fn repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("app");
    repo.write("app/__init__.py", "");
    repo.write("app/shapes.py", OLD);
    repo.write(
        "app/printer.py",
        "from app.shapes import Square\n\n\ndef summary() -> str:\n    return Square().banner()\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo
}

fn inherited(repo: &TestRepo, same_file: bool) {
    if same_file {
        repo.write("app/shapes.py", format!("{BASE}\n\n{OWNER}"));
    } else {
        repo.write("app/outline.py", BASE);
        repo.write(
            "app/shapes.py",
            format!("from app.outline import Shape\n\n\n{OWNER}"),
        );
    }
}

fn assert_preserved(repo: &TestRepo, staged: bool) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let review = repo.run_json("review", args);
    assert!(
        review["api_changes"]["removed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["name"] != "Square.banner"),
        "{review}"
    );
    assert!(
        review["api_changes"]["removed_dead"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|item| item["name"] != "Square.banner"),
        "{review}"
    );
    assert!(
        review["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "Square"),
        "{review}"
    );
    assert!(
        review["impact"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|change| change["impacted_callers"].as_array().into_iter().flatten())
            .any(|caller| caller["path"] == "app/printer.py"),
        "{review}"
    );
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(args)
        .arg("--hook")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        hook["api"]["rm"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|item| item["n"] != "Square.banner"),
        "{hook}"
    );
    assert!(
        hook["api"]["rm_dead"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|item| item["n"] != "Square.banner"),
        "{hook}"
    );
}

fn assert_removed(repo: &TestRepo, staged: bool, label: &str) {
    let args = if staged {
        &["--git", "--staged"][..]
    } else {
        &["--git"][..]
    };
    let review = repo.run_json("review", args);
    assert!(
        review["api_changes"]["removed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "Square.banner"),
        "{label}: {review}"
    );
}

fn printer_refs(repo: &TestRepo) -> Vec<serde_json::Value> {
    repo.run_json("refs", &["--name", "banner"])["refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|reference| reference["path"] == "app/printer.py")
        .cloned()
        .collect()
}

#[test]
fn python_inherited_method_in_added_local_module_is_not_removed() {
    for staged in [false, true] {
        let repo = repo();
        let before = printer_refs(&repo);
        inherited(&repo, false);
        if staged {
            repo.stage_all();
        }
        assert_preserved(&repo, staged);
        assert_eq!(before, printer_refs(&repo));
    }
}

#[test]
fn python_inherited_method_in_same_file_is_not_removed() {
    for staged in [false, true] {
        let repo = repo();
        inherited(&repo, true);
        if staged {
            repo.stage_all();
        }
        assert_preserved(&repo, staged);
    }
}

#[test]
fn python_inherited_method_keeps_actual_deletion_blocking() {
    for staged in [false, true] {
        let repo = repo();
        repo.write("app/shapes.py", OWNER.replace("(Shape)", ""));
        if staged {
            repo.stage_all();
        }
        assert_removed(&repo, staged, "actual deletion");
    }
}

#[test]
fn python_inherited_method_requires_identical_callable_and_lexical_bindings() {
    let cases = [
        (
            "required parameter",
            OLD.to_owned(),
            BASE.replace("banner(self)", "banner(self, prefix)"),
        ),
        (
            "changed body",
            OLD.to_owned(),
            BASE.replace("self.area()", "self.side"),
        ),
        (
            "string contents",
            OLD.to_owned(),
            BASE.replace("area=", "area ="),
        ),
        (
            "old module annotation",
            format!("str = 1\n{OLD}"),
            BASE.to_owned(),
        ),
        (
            "base module annotation",
            OLD.to_owned(),
            format!("str = 1\n{BASE}"),
        ),
        (
            "old class annotation",
            OLD.replace("    side =", "    str = 1\n    side ="),
            BASE.to_owned(),
        ),
        (
            "base class annotation",
            OLD.to_owned(),
            BASE.replace("class Shape:", "class Shape:\n    str = 1"),
        ),
        (
            "forward annotation",
            OLD.replace("-> str", "-> 'str'"),
            BASE.replace("-> str", "-> 'str'"),
        ),
        (
            "future annotations",
            format!("from __future__ import annotations\n{OLD}"),
            BASE.to_owned(),
        ),
        (
            "free global",
            format!("PREFIX = 'old'\n{}", OLD.replace("self.area()", "PREFIX")),
            format!("PREFIX = 'new'\n{}", BASE.replace("self.area()", "PREFIX")),
        ),
        (
            "super",
            OLD.replace("self.area()", "super().area()"),
            BASE.replace("self.area()", "super().area()"),
        ),
        (
            "class cell",
            OLD.replace("self.area()", "__class__"),
            BASE.replace("self.area()", "__class__"),
        ),
        (
            "module intrinsic",
            OLD.replace("self.area()", "__name__"),
            BASE.replace("self.area()", "__name__"),
        ),
        (
            "frame reflection",
            OLD.replace("self.area()", "self.f_globals"),
            BASE.replace("self.area()", "self.f_globals"),
        ),
    ];
    for (label, old, base) in cases {
        let repo = repo();
        if old != OLD {
            repo.write("app/shapes.py", old);
            repo.commit_all("old variant");
        }
        inherited(&repo, false);
        repo.write("app/outline.py", base);
        assert_removed(&repo, false, label);
    }
}

#[test]
fn python_inherited_method_requires_plain_unique_owners_and_base() {
    let cases = [
        (
            "unknown base",
            format!("{BASE}\n{}", OWNER.replace("(Shape)", "(Unknown)")),
        ),
        (
            "multiple bases",
            format!(
                "{BASE}\nclass Other:\n    pass\n{}",
                OWNER.replace("(Shape)", "(Shape, Other)")
            ),
        ),
        (
            "cycle",
            format!(
                "{}\n{OWNER}",
                BASE.replace("class Shape:", "class Shape(Square):")
            ),
        ),
        (
            "metaclass",
            format!(
                "{BASE}\n{}",
                OWNER.replace("(Shape)", "(Shape, metaclass=Meta)")
            ),
        ),
        ("decorated owner", format!("{BASE}\n@decorate\n{OWNER}")),
        (
            "decorated member",
            format!(
                "{}\n{OWNER}",
                BASE.replace("    def banner", "    @decorate\n    def banner")
            ),
        ),
        ("duplicate base", format!("{BASE}\n{BASE}\n{OWNER}")),
        (
            "duplicate method",
            format!(
                "{}\n{OWNER}",
                BASE.replace(
                    "    def banner",
                    "    def banner(self):\n        return 'other'\n\n    def banner"
                )
            ),
        ),
        ("member shadow", format!("{BASE}    banner = 1\n\n{OWNER}")),
        (
            "conditional base",
            format!(
                "if enabled:\n{}\n{OWNER}",
                BASE.lines()
                    .map(|line| format!("    {line}\n"))
                    .collect::<String>()
            ),
        ),
        (
            "unrelated default",
            format!("{BASE}    def other(self, value=mutate()):\n        pass\n\n{OWNER}"),
        ),
        (
            "root function",
            format!("def other(value=mutate()):\n    pass\n\n{BASE}\n{OWNER}"),
        ),
        (
            "dynamic namespace",
            format!("{BASE}\nglobals()['Shape'] = Unknown\n{OWNER}"),
        ),
    ];
    for (label, source) in cases {
        let repo = repo();
        repo.write("app/shapes.py", source);
        assert_removed(&repo, false, label);
    }
}

#[test]
fn python_inherited_method_requires_unambiguous_local_module() {
    for case in [
        "external",
        "alias",
        "relative",
        "src competitor",
        "stub",
        "initializer stub",
        "package module",
        "large initializer",
        "package effects",
        "configuration",
        "extra import",
        "preexisting base",
    ] {
        let repo = repo();
        if case == "preexisting base" {
            repo.write("app/outline.py", BASE);
            repo.commit_all("existing base");
        }
        inherited(&repo, false);
        match case {
            "external" => repo.write(
                "app/shapes.py",
                format!("from external import Shape\n{OWNER}"),
            ),
            "alias" => repo.write(
                "app/shapes.py",
                format!(
                    "from app.outline import Shape as Outline\n{}",
                    OWNER.replace("(Shape)", "(Outline)")
                ),
            ),
            "relative" => repo.write(
                "app/shapes.py",
                format!("from .outline import Shape\n{OWNER}"),
            ),
            "src competitor" => {
                repo.create_dir_all("src/app");
                repo.write("src/app/__init__.py", "");
            }
            "stub" => repo.write("app/outline.pyi", "class Shape: ...\n"),
            "initializer stub" => repo.write("app/__init__.pyi", "class Shape: ...\n"),
            "package module" => repo.create_dir_all("app/outline"),
            "large initializer" => {
                repo.write("app/__init__.py", format!("#{}\n", "x".repeat(1024 * 1024)))
            }
            "package effects" => repo.write("app/__init__.py", "from external import *\n"),
            "configuration" => repo.write("custom.pth", "other_root\n"),
            "extra import" => repo.write(
                "app/outline.py",
                format!("from external import Value\n{BASE}"),
            ),
            "preexisting base" => {}
            _ => unreachable!(),
        }
        assert_removed(&repo, false, case);
    }
}

#[test]
fn python_inherited_method_uses_index_when_worktree_diverges() {
    let valid_index = repo();
    inherited(&valid_index, false);
    valid_index.stage_all();
    valid_index.write("app/outline.py", BASE.replace("self.area()", "self.side"));
    assert_preserved(&valid_index, true);
    assert_removed(&valid_index, false, "changed worktree base");

    let invalid_index = repo();
    inherited(&invalid_index, false);
    invalid_index.write(
        "app/outline.py",
        BASE.replace("banner(self)", "banner(self, prefix)"),
    );
    invalid_index.stage_all();
    invalid_index.write("app/outline.py", BASE);
    assert_preserved(&invalid_index, false);
    assert_removed(&invalid_index, true, "changed index signature");
}

#[test]
fn python_inherited_method_keeps_source_limits_blocking() {
    for padding in [
        format!("#{}\n", "x".repeat(1024 * 1024)),
        "0\n".repeat(50_001),
    ] {
        let repo = repo();
        inherited(&repo, false);
        repo.write("app/outline.py", format!("{padding}{BASE}"));
        assert_removed(&repo, false, "proof source limit");
    }
}

#[test]
fn python_inherited_method_requires_inert_owner_packages_in_both_versions() {
    const EFFECT: &str = "from .shapes import Square\nSquare.banner = 0\n";
    for old_effect in [false, true] {
        let repo = repo();
        if old_effect {
            repo.write("app/__init__.py", EFFECT);
            repo.commit_all("old package mutation");
        }
        inherited(&repo, true);
        repo.write("app/__init__.py", if old_effect { "" } else { EFFECT });
        assert_removed(&repo, false, "owner package mutation");
    }
}

#[test]
fn python_inherited_method_uses_index_owner_package_initializers() {
    const EFFECT: &str = "from .shapes import Square\nSquare.banner = 0\n";
    let valid_index = repo();
    inherited(&valid_index, true);
    valid_index.stage_all();
    valid_index.write("app/__init__.py", EFFECT);
    assert_preserved(&valid_index, true);
    assert_removed(&valid_index, false, "working tree package mutation");

    let invalid_index = repo();
    inherited(&invalid_index, true);
    invalid_index.write("app/__init__.py", EFFECT);
    invalid_index.stage_all();
    invalid_index.write("app/__init__.py", "");
    assert_preserved(&invalid_index, false);
    assert_removed(&invalid_index, true, "index package mutation");
}

#[test]
fn python_inherited_method_keeps_src_owner_identity_ambiguity_blocking() {
    let repo = repo();
    repo.create_dir_all("src");
    std::fs::rename(repo.root().join("app"), repo.root().join("src/app")).unwrap();
    repo.write("src/__init__.py", "");
    repo.write(
        "src/app/printer.py",
        "from src.app.shapes import Square\n\ndef summary():\n    return Square().banner()\n",
    );
    repo.commit_all("src package");
    repo.write("src/app/shapes.py", format!("{BASE}\n{OWNER}"));
    repo.write(
        "src/__init__.py",
        "from .app.shapes import Square\nSquare.banner = 0\n",
    );
    assert_removed(&repo, false, "src owner package identity");
}
