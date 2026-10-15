//! Python のメンバー削除では、別クラスのフィールドと不明な受け手を区別する。

use super::support::*;

const PROPERTY: &str = "    @property\n    def label(self):\n        return self.title.upper()\n";
const METHOD: &str = "    def label(self):\n        return self.title.upper()\n";

fn member_removal(member: &str, fields: &str, caller: &str) -> std::process::Output {
    let after = format!(
        "{fields}\nclass Board:\n    def __init__(self, title):\n        self.title = title\n\n    def count(self):\n        return 1\n"
    );
    let before = after.replace(
        "    def count(self):",
        &format!("{member}\n    def count(self):"),
    );
    a3_review_hook(
        |root| std::fs::write(root.join("shop/board.py"), &after).unwrap(),
        &[
            ("shop/__init__.py", ""),
            ("shop/board.py", &before),
            ("shop/report.py", caller),
        ],
    )
}

#[test]
fn python_member_removal_reports_original_untyped_collection_as_unresolved() {
    let before = "from dataclasses import dataclass\n\n@dataclass\nclass Item:\n    slot: int\n\nclass Board:\n    def __init__(self, items):\n        self.items = items\n\n    def slot(self, number):\n        return next((item for item in self.items if item.slot == number), None)\n\n    def count(self):\n        return len(self.items)\n";
    let after = "from dataclasses import dataclass\n\n@dataclass\nclass Item:\n    slot: int\n\nclass Board:\n    def __init__(self, items):\n        self.items = items\n\n    def count(self):\n        return len(self.items)\n";
    let output = a3_review_hook(
        |root| std::fs::write(root.join("shop/board.py"), after).unwrap(),
        &[
            ("shop/__init__.py", ""),
            ("shop/board.py", before),
            (
                "shop/report.py",
                "from shop.board import Board\n\ndef total(board: Board) -> int:\n    return sum(item.slot for item in board.items)\n",
            ),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm_unresolved\""), "{stderr}");
    assert!(!stderr.contains("\"rm_dead\""), "{stderr}");
}

#[test]
fn python_member_removal_keeps_inherited_bound_methods_with_numeric_demands_blocking() {
    let after = "class Tag:\n    label: int\n\nclass Board:\n    def count(self):\n        return 1\n\nclass Combined(Board, Tag):\n    pass\n";
    let before = after.replace(
        "    def count(self):",
        &format!("{METHOD}\n    def count(self):"),
    );
    let output = a3_review_hook(
        |root| std::fs::write(root.join("shop/board.py"), after).unwrap(),
        &[
            ("shop/__init__.py", ""),
            ("shop/board.py", &before),
            (
                "shop/report.py",
                "from shop.board import Tag\n\ndef callback(tag: Tag):\n    return tag.label\n\ndef numeric(obj):\n    return -obj.label\n",
            ),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm\":"), "{stderr}");
}

#[test]
fn python_member_removal_reports_only_type_demand_positions_as_unresolved() {
    let fields = "class Tag:\n    label: int\n";
    for expression in [
        "-obj.label",
        "+obj.label",
        "~obj.label",
        "obj.label * 2",
        "2 + obj.label",
        "obj.label << 1",
        "obj.label < 0",
        "0 <= obj.label",
        "int(obj.label)",
        "float(obj.label)",
        "abs(obj.label)",
        "round(obj.label)",
        "range(obj.label)",
        "len(obj.label)",
        "sum(item.label for item in obj.items)",
        "sum([item.label for item in obj.items], 0)",
        "sum((item.label for item in obj.items), -1)",
        "sum(item.label for item in obj.items if item.label > 0)",
        "sum(item.label + 1 for item in obj.items)",
        "[item.label + 1 for item in obj.items]",
        "abs((obj.label if flag else 0))",
    ] {
        let caller = format!("def numeric(obj, flag=False):\n    return {expression}\n");
        let output = member_removal(METHOD, fields, &caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{expression}\n{stderr}");
        let json: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
        assert_eq!(
            json["api"]["rm_unresolved"][0]["confidence"], "low",
            "{expression}\n{stderr}"
        );
        assert_eq!(
            json["api"]["rm_unresolved"][0]["r"],
            "unresolved_receiver_type_demand"
        );
        assert!(json["api"].get("rm_dead").is_none(), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_callable_and_unknown_consumers_blocking() {
    for expression in [
        "obj.label",
        "bool(obj.label)",
        "obj.label == 0",
        "obj.label != 0",
        "str(obj.label)",
        "f'{obj.label}'",
        "[item.label for item in obj.items]",
        "any(item.label for item in obj.items)",
        "sorted(item.label for item in obj.items)",
        "max(item.label for item in obj.items)",
        "consume(obj.label)",
        "obj.label()",
        "obj.label + other",
        "obj.label % 2",
        "2 % obj.label",
        "sum((item.label for item in obj.items), other)",
        "sum([item.label for item in obj.items], start=0)",
        "sum([obj.label])",
        "int(obj.label, 10)",
        "int(value=obj.label)",
        "sum(item.label for item in obj.items for other in obj.items)",
    ] {
        let caller = format!("def numeric(obj, other=None):\n    return {expression}\n");
        let output = member_removal(METHOD, "class Tag:\n    label: int\n", &caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{expression}\n{stderr}");
        assert!(stderr.contains("\"rm\":"), "{expression}\n{stderr}");
    }
}

#[test]
fn python_member_removal_preserves_known_owners_exceptions_and_bindings() {
    for caller in [
        "from shop.board import Board\ndef numeric(obj: Board):\n    return -obj.label\n",
        "from shop.board import Board\ndef numeric():\n    return -Board('x').label\n",
        "from shop.board import Board\ndef numeric():\n    return -Board.label\n",
        "from shop.board import Board as Alias\ndef numeric():\n    return -Alias.label\n",
        "from shop.board import Board\ndef numeric():\n    obj = Board('x')\n    return -obj.label\n",
        "def numeric():\n    obj = factory()\n    return -obj.label\n",
        "class Reader:\n    def numeric(that):\n        return -that.label\n",
        "def numeric(self):\n    return -self.label\n",
        "def numeric(obj):\n    try:\n        return -obj.label\n    except Exception:\n        return 0\n",
        "def numeric(obj):\n    with context():\n        return -obj.label\n",
        "def numeric(obj, int):\n    return int(obj.label)\n",
        "def sum(values):\n    return values\ndef numeric(obj):\n    return sum(item.label for item in obj.items)\n",
        "from other import int\ndef numeric(obj):\n    return int(obj.label)\n",
        "import builtins\ndef numeric(obj):\n    return -obj.label\n",
        "def numeric(obj):\n    del int\n    return int(obj.label)\n",
        "def numeric(obj):\n    obj.label = 1\n    return -obj.label\n",
        "def numeric(obj):\n    return -obj.label, obj.label()\n",
        "def numeric(obj):\n    callback = obj.label\n    return -obj.label, callback\n",
    ] {
        let output = member_removal(METHOD, "class Tag:\n    label: int\n", caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\":"), "{caller}\n{stderr}");
    }
    for member in [
        PROPERTY,
        "    @staticmethod\n    def label():\n        return 1\n",
        "    @classmethod\n    def label(cls):\n        return 1\n",
    ] {
        let output = member_removal(
            member,
            "class Tag:\n    label: int\n",
            "def numeric(obj):\n    return -obj.label\n",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{member}\n{stderr}");
    }
    let output = member_removal(METHOD, "", "def numeric(obj):\n    return -obj.label\n");
    assert!(!output.status.success());
}

#[test]
fn python_member_removal_keeps_rebound_old_methods_blocking() {
    for (binding, external) in [
        ("    label = property(label)\n", ""),
        ("    label: int\n", ""),
        ("", "\nBoard.label = property(Board.label)\n"),
        ("", "\nAlias = Board\nAlias.label = property(Alias.label)\n"),
    ] {
        let after =
            "class Tag:\n    label: int\n\nclass Board:\n    def count(self):\n        return 1\n";
        let before = format!(
            "class Tag:\n    label: int\n\nclass Board:\n{METHOD}{binding}\n    def count(self):\n        return 1\n{external}"
        );
        let output = a3_review_hook(
            |root| std::fs::write(root.join("shop/board.py"), after).unwrap(),
            &[
                ("shop/__init__.py", ""),
                ("shop/board.py", &before),
                (
                    "shop/report.py",
                    "def numeric(obj):\n    return -obj.label\n",
                ),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{binding}\n{stderr}");
        assert!(stderr.contains("\"rm\":"), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_escaped_lazy_demands_blocking() {
    for caller in [
        "def numeric(items):\n    g = (item.label + 1 for item in items)\n    try:\n        return list(g)\n    except TypeError:\n        return []\n",
        "def numeric(items):\n    return (item.label + 1 for item in items)\n",
        "def numeric(items):\n    return consume(item.label + 1 for item in items)\n",
        "def numeric(items):\n    return (sum(item.label for item in items) for other in items)\n",
    ] {
        let output = member_removal(METHOD, "class Tag:\n    label: int\n", caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\":"), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_builtin_replacement_clues_blocking() {
    for caller in [
        "__builtins__.sum = consume\ndef numeric(obj):\n    return sum(item.label for item in obj.items)\n",
        "__builtins__['len'] = consume\ndef numeric(obj):\n    return len(obj.label)\n",
        "from other import *\ndef numeric(obj):\n    return sum(item.label for item in obj.items)\n",
    ] {
        let output = member_removal(METHOD, "class Tag:\n    label: int\n", caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\":"), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_known_attribute_iterables_blocking() {
    for (field, caller) in [
        (
            "    items: list[Board]\n",
            "def numeric(board: Board):\n    return sum(item.label for item in board.items)\n",
        ),
        (
            "    items: list[Board]\n",
            "def numeric(board: Board):\n    return sum(item.label for item in board.items.nested)\n",
        ),
        (
            "    def __init__(self, items: list[Board]):\n        self.items = items\n",
            "def numeric(board: Board):\n    return sum(item.label for item in board.items)\n",
        ),
        (
            "    @property\n    def items(self):\n        return [self]\n",
            "def numeric(board: Board):\n    return sum(item.label for item in board.items)\n",
        ),
        (
            "    def __init__(self, items):\n        self.items = items\n",
            "def numeric(board=Board([])):\n    return sum(item.label for item in board.items)\n",
        ),
        (
            "    def __init__(self, items):\n        self.items = items\n",
            "def numeric(board: Unknown):\n    return sum(item.label for item in board.items)\n",
        ),
    ] {
        let after = format!(
            "class Tag:\n    label: int\n\nclass Board:\n{field}\n    def count(self):\n        return 1\n"
        );
        let before = after.replace(
            "    def count(self):",
            &format!("{METHOD}\n    def count(self):"),
        );
        let caller = format!("from shop.board import Board\n{caller}");
        let output = a3_review_hook(
            |root| std::fs::write(root.join("shop/board.py"), &after).unwrap(),
            &[
                ("shop/__init__.py", ""),
                ("shop/board.py", &before),
                ("shop/report.py", &caller),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{field}\n{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\":"), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_custom_sum_accumulators_blocking() {
    let accumulator = "class Accum:\n    def __radd__(self, other):\n        return self\n    def __add__(self, other):\n        return self\n\n";
    let fields = format!("{accumulator}class Tag:\n    label: int\n");
    let output = member_removal(
        METHOD,
        &fields,
        "def numeric(items):\n    return sum(item.label for item in items)\n",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm\":"), "{stderr}");
    let output = member_removal(
        METHOD,
        &fields,
        "def numeric(obj):\n    return -obj.label\n",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm_unresolved\""), "{stderr}");
}

#[test]
fn python_member_removal_exposes_unresolved_evidence_in_full_json_and_hook() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("shop");
    repo.write("shop/__init__.py", "");
    let after =
        "class Tag:\n    label: int\n\nclass Board:\n    def count(self):\n        return 1\n";
    repo.write(
        "shop/board.py",
        after.replace(
            "    def count(self):",
            &format!("{METHOD}\n    def count(self):"),
        ),
    );
    repo.write(
        "shop/report.py",
        "def numeric(obj):\n    return -obj.label\n",
    );
    repo.commit_all("fixture");
    repo.write("shop/board.py", after);
    let full = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git"])
        .output()
        .unwrap();
    assert!(full.status.success());
    let json: serde_json::Value = serde_json::from_slice(&full.stdout).unwrap();
    let removal = &json["api_changes"]["removed_unresolved"][0];
    assert_eq!(removal["name"], "Board.label");
    assert_eq!(removal["reason"], "unresolved_receiver_type_demand");
    assert_eq!(removal["confidence"], "low");
    assert_eq!(
        removal["references"][0],
        serde_json::json!({"file":"shop/report.py","line":1,"column":16,"demand":"unary_numeric"})
    );
    assert_eq!(
        removal["field_owners"][0],
        serde_json::json!({"file":"shop/board.py","class":"Tag","line":1,"column":4})
    );
    assert!(json["api_changes"].get("uncertain_removals").is_none());
    let hook = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert!(hook.status.success());
    let json: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
    assert_eq!(
        json["api"]["rm_unresolved"][0]["refs"][0]["d"],
        "unary_numeric"
    );
    assert_eq!(json["blocking_categories"], serde_json::json!([]));
    let single = cargo_bin()
        .args(["refs", "--name", "label", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let batch = cargo_bin()
        .args(["refs", "--names", "label", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert!(single.status.success() && batch.status.success());
    assert_eq!(single.stdout, batch.stdout);
    let refs: serde_json::Value = serde_json::from_slice(&single.stdout).unwrap();
    assert_eq!(refs["refs"].as_array().unwrap().len(), 2);
    assert!(
        refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reference| reference["kind"] == "ref")
    );
    repo.stage_all();
    repo.write(
        "shop/report.py",
        "def numeric(obj):\n    return obj.label()\n",
    );
    let staged = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--staged", "--hook"])
        .output()
        .unwrap();
    assert!(
        staged.status.success(),
        "{}",
        String::from_utf8_lossy(&staged.stderr)
    );
    let staged_json: serde_json::Value = serde_json::from_slice(&staged.stderr).unwrap();
    assert!(staged_json["api"].get("rm_unresolved").is_some());
    let worktree = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert!(!worktree.status.success());
}

#[test]
fn python_member_removal_bounds_nested_numeric_literals() {
    let caller = format!(
        "def numeric(obj):\n    return obj.label + {}1\n",
        "-".repeat(256)
    );
    let output = member_removal(METHOD, "class Tag:\n    label: int\n", &caller);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm\":"), "{stderr}");
}

#[test]
fn python_member_removal_keeps_typed_and_explicit_comprehension_receivers_blocking() {
    for caller in [
        "from shop.board import Board\ndef numeric(values: list[Board]):\n    return sum(1 for obj in values if obj.label > 0)\n",
        "from shop.board import Board\ndef numeric(values: list[Board]):\n    return sum(1 for obj in (values) if obj.label > 0)\n",
        "from shop.board import Board\ndef numeric(values: list[Board]):\n    return sum(obj.label for obj in (values))\n",
        "from shop.board import Board\ndef numeric(values: list[Board]):\n    return sum(obj.label for obj in (((values))))\n",
        "from shop.board import Board\ndef numeric():\n    values: list[Board] = [Board('x')]\n    return sum(obj.label for obj in values)\n",
        "from shop.board import Board\ndef numeric():\n    return sum(obj.label for obj in [Board('x')])\n",
        "def numeric():\n    values = factory()\n    return sum(obj.label for obj in values)\n",
        "def numeric():\n    return sum(obj.label for obj in factory().items)\n",
        "def numeric():\n    values = factory()\n    return sum(obj.label for obj in values.items)\n",
        "from shop.board import Board\ndef numeric():\n    values = Board('x')\n    return sum(obj.label for obj in values.items)\n",
        "def numeric(values):\n    values = factory()\n    return sum(obj.label for obj in values.items)\n",
    ] {
        let output = member_removal(METHOD, "class Tag:\n    label: int\n", caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\":"), "{caller}\n{stderr}");
    }
    for iterable in ["values", "(values)", "(((values)))"] {
        let caller =
            format!("def numeric(values):\n    return sum(obj.label for obj in {iterable})\n");
        let output = member_removal(METHOD, "class Tag:\n    label: int\n", &caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{caller}\n{stderr}");
        assert!(stderr.contains("\"rm_unresolved\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_attributes_typed_comprehensions_to_surviving_fields() {
    let fields = "from dataclasses import dataclass\n\n@dataclass\nclass Tag:\n    label: str\n";
    for member in [PROPERTY, METHOD] {
        for caller in [
            "from shop.board import Tag\n\ndef names(tags: list[Tag]) -> list[str]:\n    return [tag.label for tag in tags]\n",
            "from shop.board import Tag\n\ndef names(tags: list[Tag]):\n    return tuple(tag.label for tag in tags)\n",
            "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
            "from shop.board import Tag\n\ndef name():\n    return Tag('first').label\n",
            "from shop.board import Tag\n\ndef name():\n    tag = Tag('first')\n    return tag.label\n",
        ] {
            let output = member_removal(member, fields, caller);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{caller}\n{stderr}");
            assert!(stderr.contains("\"rm_dead\""), "{stderr}");
            assert!(stderr.contains("Board.label"), "{stderr}");
        }
    }
}

#[test]
fn python_member_removal_keeps_unknown_receivers_and_bound_methods_blocking() {
    let fields = "from dataclasses import dataclass\n\n@dataclass\nclass Tag:\n    label: str\n";
    for member in [PROPERTY, METHOD] {
        for caller in [
            "def name(obj):\n    return obj.label\n",
            "def name(obj):\n    callback = obj.label\n    return callback\n",
            "def name(obj):\n    return obj.label()\n",
            "from shop.board import Board\n\ndef name(board: Board):\n    return board.label\n",
            "from shop.board import Board\n\ndef name(board: Board):\n    callback = board.label\n    return callback\n",
            "from shop.board import Board\n\ndef name():\n    return Board('first').label()\n",
            "from shop.board import Tag\n\ndef names(tags):\n    return [tag.label for tag in tags]\n",
        ] {
            let output = member_removal(member, fields, caller);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{caller}\n{stderr}");
            assert!(stderr.contains("\"rm\""), "{stderr}");
            assert!(stderr.contains("Board.label"), "{stderr}");
        }
    }
}

#[test]
fn python_member_removal_does_not_treat_a_field_declaration_as_a_caller() {
    for fields in [
        "class Tag:\n    label: str\n",
        "class Tag:\n    label = 'first'\n",
        "class Tag:\n    def __init__(self):\n        self.label = 'first'\n",
    ] {
        let output = member_removal(METHOD, fields, "");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{fields}\n{stderr}");
        assert!(stderr.contains("\"rm_dead\""), "{stderr}");
    }
    let output = member_removal(
        METHOD,
        "class Tag:\n    def __init__(self):\n        self.label = 'first'\n",
        "from shop.board import Tag\n\ndef name():\n    return Tag().label\n",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm_dead\""), "{stderr}");
}

#[test]
fn python_member_removal_rejects_ambiguous_classes_and_receiver_bindings() {
    for (fields, caller) in [
        (
            "class Tag(Base):\n    label: str\n",
            "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
        ),
        (
            "@decorate\nclass Tag:\n    label: str\n",
            "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
        ),
        (
            "class Tag:\n    label: str\n    def __getattribute__(self, name):\n        return lookup(name)\n",
            "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
        ),
        (
            "class Tag:\n    label: str\n",
            "from shop.board import Tag, Board\n\ndef name():\n    tag = Tag()\n    tag = Board('first')\n    return tag.label\n",
        ),
        (
            "class Tag:\n    label: str\n",
            "from shop.board import Tag, Board\n\ndef names(tags: list[Tag]):\n    tags = [Board('first')]\n    return [tag.label for tag in tags]\n",
        ),
        (
            "class Tag:\n    label: str\n",
            "from shop.board import Tag as Alias\n\ndef name(tag: Alias):\n    return tag.label\n",
        ),
        (
            "class Tag:\n    label: str\n",
            "from shop.board import Tag\n\ndef name(tag: Tag):\n    Tag.label = other\n    return tag.label\n",
        ),
        (
            "class Tag:\n    label: str\n",
            "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label()\n",
        ),
    ] {
        let output = member_removal(PROPERTY, fields, caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{fields}\n{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_inherited_owners_and_untyped_collections_blocking() {
    let before = "class Tag:\n    label: str\n\nclass Board(Tag):\n    @property\n    def label(self):\n        return 'first'\n\n    def count(self):\n        return 1\n";
    let after =
        "class Tag:\n    label: str\n\nclass Board(Tag):\n    def count(self):\n        return 1\n";
    let output = a3_review_hook(
        |root| std::fs::write(root.join("shop/board.py"), after).unwrap(),
        &[
            ("shop/__init__.py", ""),
            ("shop/board.py", before),
            (
                "shop/report.py",
                "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
            ),
        ],
    );
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 型のない collection の値をそのまま返すと、bound method も外へ出せる。
    let before = "from dataclasses import dataclass\n\n@dataclass\nclass Item:\n    slot: int\n\nclass Board:\n    def __init__(self, items):\n        self.items = items\n    def slot(self, number):\n        return next((item for item in self.items if item.slot == number), None)\n    def count(self):\n        return len(self.items)\n";
    let after = before.replace("    def slot(self, number):\n        return next((item for item in self.items if item.slot == number), None)\n", "");
    let output = a3_review_hook(
        |root| std::fs::write(root.join("shop/board.py"), &after).unwrap(),
        &[
            ("shop/__init__.py", ""),
            ("shop/board.py", before),
            (
                "shop/report.py",
                "from shop.board import Board\n\ndef total(board: Board):\n    return tuple(item.slot for item in board.items)\n",
            ),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Board.slot") && stderr.contains("\"rm\""),
        "{stderr}"
    );
}

#[test]
fn python_member_removal_preserves_field_occurrences_in_single_and_batch_refs() {
    let repo = TestRepo::new();
    repo.create_dir_all("shop");
    repo.write("shop/__init__.py", "");
    repo.write(
        "shop/board.py",
        "from dataclasses import dataclass\n\n@dataclass\nclass Tag:\n    label: str\n",
    );
    repo.write("shop/report.py", "from shop.board import Tag\n\ndef names(tags: list[Tag]):\n    return [tag.label for tag in tags]\n");
    let single = repo.run_json("refs", &["--name", "label"]);
    let batch = repo.run_json("refs", &["--names", "label"]);
    assert_eq!(single, batch);
    let references = single["refs"].as_array().unwrap();
    assert_eq!(references.len(), 2, "{single}");
    assert!(
        references
            .iter()
            .all(|reference| reference["kind"] == "ref"),
        "{single}"
    );
}

#[test]
fn python_member_removal_rejects_default_values_global_bindings_and_type_parameters() {
    let fields = "from dataclasses import dataclass\n\n@dataclass\nclass Tag:\n    label: str\n";
    for caller in [
        "from shop.board import Board, Tag\n\ntag = Board('first')\ndef name(tag: Tag, default=tag.label):\n    return default\n",
        "from shop.board import Tag\nfrom external import tags\n\ndef names(tags: list[Tag], default=[tag.label for tag in tags]):\n    return default\n",
        "from shop.board import Tag\nfrom external import swap\n\ndef name():\n    global tag\n    tag = Tag('first')\n    swap()\n    return tag.label\n",
        "from shop.board import Tag\nfrom external import swap\n\ndef outer():\n    tag = None\n    def name():\n        nonlocal tag\n        tag = Tag('first')\n        swap()\n        return tag.label\n    return name()\n",
        "from shop.board import Tag\n\ndef name[Tag](tag: Tag):\n    return tag.label\n",
        "from shop.board import Tag\nfrom external import Other\n\ntype Tag = Other\ndef name(tag: Tag):\n    return tag.label\n",
    ] {
        let output = member_removal(PROPERTY, fields, caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_rejects_assigned_dynamic_hooks() {
    for hook in ["__new__", "__getattribute__", "__getattr__"] {
        let fields = format!("class Tag:\n    label: str\n    {hook} = factory\n");
        let output = member_removal(
            PROPERTY,
            &fields,
            "from shop.board import Tag\n\ndef name():\n    return Tag().label\n",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{fields}\n{stderr}");
        assert!(stderr.contains("\"rm\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_combined_subclasses_and_class_aliases_blocking() {
    let fields = "class Tag:\n    label: str\n";
    for extra in [
        "class Combined(Board, Tag):\n    pass\n",
        "Alias = Board\nclass Combined(Alias, Tag):\n    pass\n",
        "Combined = type('Combined', (Board, Tag), {})\n",
        "class Combined(type(Board('first')), Tag):\n    pass\n",
        "def owner():\n    return Board\n",
    ] {
        let caller = format!(
            "from shop.board import Board, Tag\n\n{extra}\ndef name(tag: Tag):\n    return tag.label\n"
        );
        let output = member_removal(PROPERTY, fields, &caller);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{caller}\n{stderr}");
        assert!(stderr.contains("\"rm\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_requires_complete_reference_input() {
    for (path, source) in [
        ("shop/broken.py", "def broken(:\n"),
        (
            "shop/view.vue",
            "<template>{{ current.label }}</template>\n",
        ),
        (
            "shop/generated.py",
            "# Code generated by a tool. DO NOT EDIT.\nclass Generated:\n    pass\n",
        ),
    ] {
        let before = "class Tag:\n    label: str\n\nclass Board:\n    @property\n    def label(self):\n        return 'first'\n\n    def count(self):\n        return 1\n";
        let after =
            "class Tag:\n    label: str\n\nclass Board:\n    def count(self):\n        return 1\n";
        let output = a3_review_hook(
            |root| std::fs::write(root.join("shop/board.py"), after).unwrap(),
            &[
                ("shop/__init__.py", ""),
                ("shop/board.py", before),
                (
                    "shop/report.py",
                    "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
                ),
                (path, source),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{path}\n{stderr}");
        assert!(stderr.contains("\"rm\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_keeps_subclasses_in_another_file_blocking() {
    let before = "class Tag:\n    label: str\n\nclass Board:\n    @property\n    def label(self):\n        return 'first'\n\n    def count(self):\n        return 1\n";
    let after =
        "class Tag:\n    label: str\n\nclass Board:\n    def count(self):\n        return 1\n";
    for subclass in [
        "from shop.board import Board, Tag\n\nclass Combined(Board, Tag):\n    pass\n",
        "from shop.board import Board as Parent, Tag\n\nclass Combined(Parent, Tag):\n    pass\n",
    ] {
        let output = a3_review_hook(
            |root| std::fs::write(root.join("shop/board.py"), after).unwrap(),
            &[
                ("shop/__init__.py", ""),
                ("shop/board.py", before),
                (
                    "shop/report.py",
                    "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
                ),
                ("shop/derived.py", subclass),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{subclass}\n{stderr}");
        assert!(stderr.contains("\"rm\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_withholds_proof_when_a_source_cannot_be_read() {
    let before = "class Tag:\n    label: str\n\nclass Board:\n    @property\n    def label(self):\n        return 'first'\n\n    def count(self):\n        return 1\n";
    let after =
        "class Tag:\n    label: str\n\nclass Board:\n    def count(self):\n        return 1\n";
    let output = a3_review_hook(
        |root| {
            std::fs::write(root.join("shop/board.py"), after).unwrap();
            std::fs::File::create(root.join("shop/oversized.py"))
                .unwrap()
                .set_len(100 * 1024 * 1024 + 1)
                .unwrap();
        },
        &[
            ("shop/__init__.py", ""),
            ("shop/board.py", before),
            (
                "shop/report.py",
                "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
            ),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("\"rm\""), "{stderr}");
}

#[test]
fn python_member_removal_rejects_reflection_without_owner_name_occurrences() {
    let before = "class Tag:\n    label: str\n\nclass Board:\n    @property\n    def label(self):\n        return 'first'\n\n    def count(self):\n        return 1\n";
    let after =
        "class Tag:\n    label: str\n\nclass Board:\n    def count(self):\n        return 1\n";
    for dynamic in [
        "import shop.board as module\nOwner = getattr(module, 'Bo' + 'ard')\n",
        "import operator\nOwner = operator.attrgetter('Bo' + 'ard')(module)\n",
        "Owner = current.__mro__[0]\n",
        "Owner = current.__subclasses__()[0]\n",
        "Owner = type(current)\n",
        "import pkgutil, shop.board\nOwner = pkgutil.resolve_name('shop.board:Board')\n",
        "import inspect, shop.board\nOwner = dict(inspect.getmembers(shop.board))['Board']\n",
        "Owner = lookup(module, 'Bo' + 'ard')\nclass Combined(Owner, Tag):\n    pass\n",
        "class Combined(factory(), Tag):\n    pass\n",
    ] {
        let output = a3_review_hook(
            |root| std::fs::write(root.join("shop/board.py"), after).unwrap(),
            &[
                ("shop/__init__.py", ""),
                ("shop/board.py", before),
                (
                    "shop/report.py",
                    "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
                ),
                ("shop/dynamic.py", dynamic),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{dynamic}\n{stderr}");
        assert!(stderr.contains("\"rm\""), "{stderr}");
    }
}

#[test]
fn python_member_removal_accepts_local_static_bases_but_keeps_imported_bases_unproven() {
    let tag = "from dataclasses import dataclass\n\n@dataclass\nclass Tag:\n    label: str\n";
    for (bases, informational) in [
        (
            "class Base:\n    pass\nclass Child(Base):\n    pass\n",
            true,
        ),
        (
            "from external import Base\nclass Child(Base):\n    pass\n",
            false,
        ),
        ("Base = factory()\nclass Child(Base):\n    pass\n", false),
    ] {
        let fields = format!("{tag}\n{bases}");
        let output = member_removal(
            PROPERTY,
            &fields,
            "from shop.board import Tag\n\ndef name(tag: Tag):\n    return tag.label\n",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), informational, "{fields}\n{stderr}");
        assert_eq!(stderr.contains("\"rm_dead\""), informational, "{stderr}");
    }
}
