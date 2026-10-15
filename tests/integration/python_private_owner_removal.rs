//! 閉じた非公開クラスの移設と、公開経路の残る削除を区別する。

use super::support::*;

const BEFORE: &str = "class _Layout:\n    def __init__(self, items):\n        self.items = items\n\n    def width(self):\n        return len(self.items)\n\n    def build(self):\n        return '-' * self.width()\n\ndef draw_bars(items):\n    return _Layout(items).build()\n";
const AFTER: &str = "class _Frame:\n    def __init__(self, items):\n        self.items = items\n\n    def width(self):\n        return len(self.items)\n\nclass _BarLayout(_Frame):\n    def build(self):\n        return '-' * self.width()\n\ndef draw_bars(items):\n    return _BarLayout(items).build()\n";

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
