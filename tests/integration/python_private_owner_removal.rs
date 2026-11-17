//! 閉じた非公開クラスの移設と、公開経路の残る削除を区別する。

use super::support::*;

const BEFORE: &str = "class _Layout:\n    def __init__(self, items):\n        self.items = items\n\n    def width(self):\n        return len(self.items)\n\n    def build(self):\n        return '-' * self.width()\n\ndef draw_bars(items):\n    return _Layout(items).build()\n";
const AFTER: &str = "class _Frame:\n    def __init__(self, items):\n        self.items = items\n\n    def width(self):\n        return len(self.items)\n\nclass _BarLayout(_Frame):\n    def build(self):\n        return '-' * self.width()\n\ndef draw_bars(items):\n    return _BarLayout(items).build()\n";

const DELETED_CLASS: &str = "class _Table:\n    def add(self, key):\n        return key\n\n    def fuse(self, other):\n        return other\n\ndef count(values):\n    table = _Table()\n    for value in values:\n        table.add(value)\n    table.fuse(None)\n    return 0\n";
const DELETED_CLASS_AFTER: &str = "def count(values):\n    return sorted(values)\n";
const UNRELATED_CALLS: &str = "def unique(values):\n    seen = set()\n    for value in values:\n        seen.add(value)\n    return seen\n\ndef combine(left, right):\n    return left.fuse(right)\n";

fn deletion_fixture(before: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("pkg");
    repo.write("pkg/__init__.py", "");
    repo.write("pkg/table.py", before);
    repo.write("pkg/seen.py", UNRELATED_CALLS);
    repo.commit_all("fixture");
    repo
}

fn assert_deleted_class_closed(repo: &TestRepo, staged: bool) {
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
            .is_empty(),
        "{review}"
    );
    for name in ["_Table.add", "_Table.fuse"] {
        assert!(
            review["api_changes"]["removed_dead"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == name),
            "{review}"
        );
    }
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(args)
        .arg("--hook")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(hook["blocking_categories"], serde_json::json!([]), "{hook}");
    for name in ["_Table.add", "_Table.fuse"] {
        assert!(
            hook["api"]["rm_dead"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["n"] == name),
            "{hook}"
        );
    }
}

fn assert_deleted_class_blocked(repo: &TestRepo, owner: &str) {
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    for member in ["add", "fuse"] {
        assert!(
            hook["api"]["rm"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["n"] == format!("{owner}.{member}")),
            "{hook}"
        );
    }
}

#[test]
fn python_private_owner_removal_deletes_a_closed_class_without_replacement() {
    for delete_file in [false, true] {
        let repo = deletion_fixture(DELETED_CLASS);
        if delete_file {
            repo.remove_file("pkg/table.py");
        } else {
            repo.write("pkg/table.py", DELETED_CLASS_AFTER);
        }
        assert_deleted_class_closed(&repo, false);
        for name in ["add", "fuse"] {
            let single = repo.run_json("refs", &["--name", name]);
            let batch = repo.run_json("refs", &["--names", name]);
            assert_eq!(single, batch);
            let refs = single["refs"].as_array().unwrap();
            assert_eq!(refs.len(), 1, "{single}");
            assert_eq!(refs[0]["path"], "pkg/seen.py", "{single}");
            assert_eq!(refs[0]["kind"], "ref", "{single}");
        }
    }
}

#[test]
fn python_private_owner_removal_deletes_a_closed_class_from_the_index() {
    for delete_file in [false, true] {
        let repo = deletion_fixture(DELETED_CLASS);
        if delete_file {
            repo.remove_file("pkg/table.py");
        } else {
            repo.write("pkg/table.py", DELETED_CLASS_AFTER);
        }
        repo.stage_all();
        repo.write(
            "pkg/table.py",
            "def count(values):\n    return _Table().add(values)\n",
        );
        assert_deleted_class_closed(&repo, true);
        assert_deleted_class_blocked(&repo, "_Table");
    }
}

#[test]
fn python_private_owner_removal_keeps_pure_deletion_public_paths_blocking() {
    let repo = deletion_fixture(&DELETED_CLASS.replace("_Table", "Table"));
    repo.write(
        "pkg/public.py",
        "from pkg.table import Table\n\ndef use():\n    return Table().add(1), Table().fuse(None)\n",
    );
    repo.commit_all("public caller");
    repo.write("pkg/table.py", DELETED_CLASS_AFTER);
    assert_deleted_class_blocked(&repo, "Table");

    let mut cases = vec![DELETED_CLASS.replace(
        "    table.fuse(None)\n    return 0",
        "    return table.fuse(table)",
    )];
    for extra in [
        "Public = _Table\n",
        "def create():\n    return _Table()\n",
        "def create():\n    table = _Table()\n    return lambda: table.add(1)\n",
        "def consume(table: '_Table'):\n    return table.add(1)\n",
    ] {
        cases.push(format!("{DELETED_CLASS}\n{extra}"));
    }
    for before in cases {
        let repo = deletion_fixture(&before);
        repo.write("pkg/table.py", DELETED_CLASS_AFTER);
        assert_deleted_class_blocked(&repo, "_Table");
    }
}

#[test]
fn python_private_owner_removal_keeps_unproven_pure_deletions_blocking() {
    for extra in [
        "from pkg.table import _Table\n",
        "def reflect():\n    return getattr(object(), 'add')\n",
        "def broken(:\n    pass\n",
    ] {
        let repo = deletion_fixture(DELETED_CLASS);
        repo.write("pkg/unproven.py", extra);
        repo.commit_all("unproven sources");
        repo.write("pkg/table.py", DELETED_CLASS_AFTER);
        assert_deleted_class_blocked(&repo, "_Table");
    }
}

fn refactor(before: &str, after: &str, extras: &[(&str, &str)]) -> std::process::Output {
    let mut files = vec![("draw/__init__.py", ""), ("draw/chart.py", before)];
    files.extend_from_slice(extras);
    a3_review_hook(
        |root| std::fs::write(root.join("draw/chart.py"), after).unwrap(),
        &files,
    )
}

fn assert_closed(output: std::process::Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm_dead\""), "{stderr}");
    assert!(!stderr.contains("\"rm\":"), "{stderr}");
}

#[test]
fn python_private_owner_removal_splits_a_closed_class() {
    assert_closed(refactor(BEFORE, AFTER, &[]));
    assert_closed(refactor(
        BEFORE,
        AFTER,
        &[("draw/other.py", "def use(obj):\n    return obj.width()\n")],
    ));
}

#[test]
fn python_private_owner_removal_moves_a_class_with_a_changed_return_type() {
    let before = "class _Line:\n    def __init__(self, items: list[int]) -> None:\n        self.items = items\n\n    def mark(self, item: int) -> str:\n        return '-' if item else ' '\n\n    def build(self) -> str:\n        return ''.join(self.mark(item) for item in self.items)\n\ndef draw_line(items: list[int]) -> str:\n    return _Line(items).build()\n";
    let layout = "class _Line:\n    def __init__(self, items: list[int]) -> None:\n        self.items = items\n\n    def mark(self, item: int) -> str:\n        return '-' if item else ' '\n\n    def build(self) -> list[str]:\n        return [self.mark(item) for item in self.items]\n\ndef plan_line(items: list[int]) -> list[str]:\n    return _Line(items).build()\n";
    assert_closed(a3_review_hook(
        |root| {
            std::fs::write(root.join("shapes/layout.py"), layout).unwrap();
            std::fs::write(root.join("shapes/text.py"), "from shapes import layout\n\ndef draw_line(items: list[int]) -> str:\n    return ''.join(layout.plan_line(items))\n").unwrap();
        },
        &[("shapes/__init__.py", ""), ("shapes/text.py", before)],
    ));
}

#[test]
fn python_private_owner_removal_moves_and_renames_a_private_base() {
    let before = "class _Base:\n    def __init__(self, items: list[int]) -> None:\n        self.items = items\n\n    def count(self) -> int:\n        return len(self.items)\n\nclass _Line(_Base):\n    def build(self) -> str:\n        return '-' * self.count()\n\ndef draw_line(items: list[int]) -> str:\n    return _Line(items).build()\n";
    let layout = before
        .replace("_Base", "_Root")
        .replace("draw_line", "plan_line");
    assert_closed(a3_review_hook(
        |root| {
            std::fs::write(root.join("shapes/layout.py"), &layout).unwrap();
            std::fs::write(root.join("shapes/text.py"), "from shapes import layout\n\ndef draw_line(items: list[int]) -> str:\n    return layout.plan_line(items)\n").unwrap();
        },
        &[("shapes/__init__.py", ""), ("shapes/text.py", before)],
    ));
}

fn assert_blocked(output: std::process::Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm\":"), "{stderr}");
}

#[test]
fn python_private_owner_removal_preserves_public_contracts() {
    assert_blocked(refactor(
        &BEFORE.replace("_Layout", "Layout"),
        &AFTER
            .replace("_Frame", "Frame")
            .replace("_BarLayout", "BarLayout"),
        &[],
    ));
    for (old, new) in [
        ("Public = _Layout\n", "Public = _Frame\n"),
        (
            "int = '_Layout'\ndef width(value: int):\n    return value.width()\n",
            "def width(value):\n    return value.width()\n",
        ),
        (
            "Alias = '_Layout'\ndef width(value: Alias):\n    return value.width()\n",
            "def width(value):\n    return value.width()\n",
        ),
        (
            "def width(value: '_Lay' 'out'):\n    return value.width()\n",
            "def width(value):\n    return value.width()\n",
        ),
        (
            "def width(value: '_Lay' + 'out'):\n    return value.width()\n",
            "def width(value):\n    return value.width()\n",
        ),
        (
            "def create(items):\n    layout = _Layout(items)\n    return lambda: layout.width()\n",
            "def create(items):\n    return lambda: 1\n",
        ),
        (
            "def create(items):\n    layout = _Layout(items)\n    def inner():\n        return layout.width()\n    return inner\n",
            "def create(items):\n    return lambda: 1\n",
        ),
        (
            "def create(items):\n    layout = _Layout(items)\n    return (layout.width() for item in items)\n",
            "def create(items):\n    return iter(items)\n",
        ),
        (
            "__all__ = ['draw_bars', '_Layout']\n",
            "__all__ = ['draw_bars']\n",
        ),
        (
            "class Public(_Layout):\n    pass\n",
            "class Public(_BarLayout):\n    pass\n",
        ),
        (
            "def create(items):\n    return _Layout(items)\n",
            "def create(items):\n    return _Frame(items)\n",
        ),
        (
            "def create(items):\n    result = _Layout(items)\n    return result\n",
            "def create(items):\n    return _Frame(items)\n",
        ),
        (
            "def create(items):\n    return _Layout(items).build\n",
            "def create(items):\n    return _BarLayout(items).build\n",
        ),
        (
            "def width(value: '_Layout') -> int:\n    return value.width()\n",
            "def width(value: '_Frame') -> int:\n    return value.width()\n",
        ),
        (
            "def widths(values: list['_Layout']):\n    return [value.width() for value in values]\n",
            "def widths(values):\n    return [value.width() for value in values]\n",
        ),
        (
            "def widths(values: 'list[_Layout]'):\n    return [value.width() for value in values]\n",
            "def widths(values):\n    return [value.width() for value in values]\n",
        ),
        (
            "def width(value: '\\x5fLayout'):\n    return value.width()\n",
            "def width(value):\n    return value.width()\n",
        ),
    ] {
        let output = refactor(&format!("{BEFORE}\n{old}"), &format!("{AFTER}\n{new}"), &[]);
        assert_blocked(output);
    }
}

#[test]
fn python_private_owner_removal_rejects_instance_escape_and_captures() {
    for extra in [
        "    def expose(self):\n        return self\n",
        "    def expose(self):\n        alias = self\n        return alias\n",
        "    def expose(self):\n        return register(self)\n",
        "    def expose(self):\n        self.items = [self]\n        return self.items\n",
        "    def expose(self):\n        return self.width\n",
        "    def expose(self):\n        return lambda: self.width()\n",
        "    def expose(self):\n        return (self.width() for item in self.items)\n",
        "    def expose(self):\n        return consume(self.width() for item in self.items)\n",
        "    @property\n    def expose(self):\n        return self.items\n",
        "    @classmethod\n    def expose(cls):\n        return cls\n",
        "    expose = width\n",
    ] {
        let before = BEFORE.replace("\ndef draw_bars", &format!("\n{extra}\ndef draw_bars"));
        assert_blocked(refactor(&before, AFTER, &[]));
    }
}

#[test]
fn python_private_owner_removal_keeps_old_bindings_and_callers_blocking() {
    for extra in [
        "_Layout = _Frame\n",
        "__all__ = ['_Layout']\n",
        "int = '_Layout'\ndef stale(value: int):\n    return value.width()\n",
        "def stale(value: '_Lay' 'out'):\n    return value.width()\n",
        "def stale(value: '_Lay' + 'out'):\n    return value.width()\n",
        "Alias = '_Layout'\ndef stale(value: Alias):\n    return value.width()\n",
        "from external import _Layout\n",
        "def stale(items):\n    return _Layout(items).width()\n",
        "def stale(value: '_Layout'):\n    return value.width()\n",
        "def stale(value: '\\x5fLayout'):\n    return value.width()\n",
    ] {
        assert_blocked(refactor(BEFORE, &format!("{AFTER}\n{extra}"), &[]));
    }
    let retained = BEFORE.replace(
        "    def width(self):\n        return len(self.items)\n\n",
        "",
    );
    assert_blocked(refactor(BEFORE, &retained, &[]));
    assert_blocked(refactor(
        BEFORE,
        AFTER,
        &[(
            "draw/consumer.py",
            "from draw.chart import _Layout\n\ndef width(value):\n    return value.width()\n",
        )],
    ));
}

#[test]
fn python_private_owner_removal_does_not_use_a_local_class_for_qualified_legacy_names() {
    let output = a3_review_hook(
        |root| {
            std::fs::write(
                root.join("draw/chart.py"),
                "from draw import layout\n\ndef draw_bars(items):\n    return layout.plan(items)\n",
            )
            .unwrap();
            std::fs::write(root.join("draw/layout.py"), "from draw import chart as legacy\n\nclass _Layout:\n    def width(self) -> int:\n        return 1\n    def build(self) -> str:\n        return '-'\n\ndef plan(items):\n    return legacy._Layout(items).build()\n").unwrap();
        },
        &[("draw/__init__.py", ""), ("draw/chart.py", BEFORE)],
    );
    assert_blocked(output);
}

#[test]
fn python_private_owner_removal_requires_static_complete_sources() {
    for extra in [
        "def broken(:\n    pass\n",
        "def reflect():\n    return globals()\n",
        "from external import Base\nclass Other(Base):\n    pass\n",
        "class Other(factory()):\n    pass\n",
        "class Other:\n    pass\nclass Other:\n    pass\n",
        "from external import *\n",
    ] {
        assert_blocked(refactor(BEFORE, AFTER, &[("draw/other.py", extra)]));
        assert_blocked(refactor(BEFORE, &format!("{AFTER}\n{extra}"), &[]));
    }
    let oversized = format!("# {}\n", "x".repeat(1_000_000));
    assert_blocked(refactor(BEFORE, AFTER, &[("draw/other.py", &oversized)]));
    assert_blocked(refactor(
        BEFORE,
        AFTER,
        &[("draw/other.pyi", "class Other: ...\n")],
    ));
}

#[test]
fn python_private_owner_removal_accepts_a_receiver_only_local() {
    let before = BEFORE.replace(
        "return _Layout(items).build()",
        "layout = _Layout(items)\n    return layout.build()",
    );
    assert_closed(refactor(&before, AFTER, &[]));
}

#[test]
fn python_private_owner_removal_keeps_refs_occurrences() {
    let repo = TestRepo::new();
    repo.create_dir_all("draw");
    repo.write("draw/chart.py", AFTER);
    let single = repo.run_json("refs", &["--name", "width"]);
    let batch = repo.run_json("refs", &["--names", "width"]);
    assert_eq!(single, batch);
    assert_eq!(single["refs"].as_array().unwrap().len(), 2, "{single}");
}

#[test]
fn python_private_owner_removal_does_not_prove_isolation_from_a_subdirectory() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("draw");
    repo.write("draw/__init__.py", "");
    repo.write("draw/chart.py", BEFORE);
    repo.commit_all("fixture");
    repo.write("draw/chart.py", AFTER);
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.path("draw"))
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert_blocked(output);
}

#[cfg(unix)]
#[test]
fn python_private_owner_removal_retains_symbolic_link_uncertainty() {
    for old_link in [false, true] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("draw");
        repo.write("draw/__init__.py", "");
        repo.write("draw/chart.py", BEFORE);
        if old_link {
            std::os::unix::fs::symlink("chart.py", repo.path("draw/alias.py")).unwrap();
        }
        repo.commit_all("fixture");
        repo.write("draw/chart.py", AFTER);
        if !old_link {
            std::os::unix::fs::symlink("chart.py", repo.path("draw/alias.py")).unwrap();
        }
        let output = cargo_bin()
            .args(["review", "--dir"])
            .arg(repo.root())
            .args(["--git", "--hook"])
            .output()
            .unwrap();
        assert_blocked(output);
    }
}

#[test]
fn python_private_owner_removal_keeps_missing_methods_on_replacement_instances_blocking() {
    for owner in ["_Layout", "_Replacement"] {
        for direct in [false, true] {
            let layout = format!(
                "class {owner}:\n    def __init__(self, items):\n        self.items = items\n    def build(self):\n        return '-' * self.width()\n\nclass Other:\n    def width(self):\n        return 1\n\ndef plan(items):\n    return {owner}(items).{}()\n",
                if direct { "width" } else { "build" }
            );
            let output = a3_review_hook(
                |root| {
                    std::fs::write(root.join("draw/chart.py"), "from draw import layout\n\ndef draw_bars(items):\n    return layout.plan(items)\n").unwrap();
                    std::fs::write(root.join("draw/layout.py"), &layout).unwrap();
                },
                &[("draw/__init__.py", ""), ("draw/chart.py", BEFORE)],
            );
            assert_blocked(output);
        }
    }
}

#[test]
fn python_private_owner_removal_rejects_replacement_member_shadowing() {
    for after in [
        AFTER.replace(
            "        self.items = items",
            "        self.items, self.width = items, None",
        ),
        AFTER.replace(
            "class _BarLayout(_Frame):",
            "class _BarLayout(_Frame):\n    width = None",
        ),
        AFTER.replace(
            "        self.items = items",
            "        self.items = items\n        self.width = None",
        ),
        AFTER.replace(
            "    def width(self):",
            "    @property\n    def width(self):",
        ),
    ] {
        assert_blocked(refactor(BEFORE, &after, &[]));
    }
}

#[test]
fn python_private_owner_removal_requires_the_current_candidate_file_to_be_inventoried() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all(".hidden");
    repo.write(".hidden/chart.py", BEFORE);
    repo.write("other.py", "class Other:\n    def width(self):\n        return 1\n    def build(self):\n        return self.width()\n");
    repo.commit_all("fixture");
    repo.write(
        ".hidden/chart.py",
        BEFORE.replace(
            "    def width(self):\n        return len(self.items)\n\n",
            "",
        ),
    );
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert_blocked(output);
}

#[test]
fn python_private_owner_removal_keeps_non_call_references_to_missing_members_blocking() {
    for direct in [false, true] {
        let layout = format!(
            "class _Replacement:\n    def __init__(self, items):\n        self.items = items\n    def build(self):\n        return self.width\n\nclass Other:\n    def width(self):\n        return 1\n\ndef plan(items):\n    return _Replacement(items).{}\n",
            if direct { "width" } else { "build()" }
        );
        let output = a3_review_hook(
            |root| {
                std::fs::write(root.join("draw/chart.py"), "from draw import layout\n\ndef draw_bars(items):\n    return layout.plan(items)\n").unwrap();
                std::fs::write(root.join("draw/layout.py"), &layout).unwrap();
            },
            &[("draw/__init__.py", ""), ("draw/chart.py", BEFORE)],
        );
        assert_blocked(output);
    }
}

#[test]
fn python_private_owner_removal_keeps_uninitialized_subclass_fields_blocking() {
    let layout = "class _Base:\n    def __init__(self, items):\n        self.width = 1\n    def build(self):\n        return self.width\n\nclass _Replacement(_Base):\n    def __init__(self, items):\n        self.height = 2\n\nclass Other:\n    def width(self):\n        return 1\n\ndef plan(items):\n    return _Replacement(items).build()\n";
    let output = a3_review_hook(
        |root| {
            std::fs::write(
                root.join("draw/chart.py"),
                "from draw import layout\n\ndef draw_bars(items):\n    return layout.plan(items)\n",
            )
            .unwrap();
            std::fs::write(root.join("draw/layout.py"), layout).unwrap();
        },
        &[("draw/__init__.py", ""), ("draw/chart.py", BEFORE)],
    );
    assert_blocked(output);
}

#[test]
fn python_private_owner_removal_keeps_subclass_field_shadows_blocking() {
    let after = "class _Frame:\n    def width(self):\n        return len(self.items)\n    def build(self):\n        return '-' * self.width()\n\nclass _Replacement(_Frame):\n    def __init__(self, items):\n        self.items = items\n        self.width = None\n\ndef draw_bars(items):\n    return _Replacement(items).build()\n";
    assert_blocked(refactor(BEFORE, after, &[]));
}

#[test]
fn python_private_owner_removal_keeps_member_write_targets_blocking() {
    for write in [
        "_Frame.width = None\n",
        "_Frame.width += 1\n",
        "for _Frame.width in [None]:\n    pass\n",
        "[None for _Frame.width in [None]]\n",
        "with context() as _Frame.width:\n    pass\n",
        "del _Frame.width\n",
    ] {
        assert_blocked(refactor(BEFORE, &format!("{AFTER}\n{write}"), &[]));
    }
}
