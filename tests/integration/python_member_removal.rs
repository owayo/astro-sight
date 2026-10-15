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

    // 型のない collection は削除 owner の instance を含む可能性が残る。
    let before = "from dataclasses import dataclass\n\n@dataclass\nclass Item:\n    slot: int\n\nclass Board:\n    def __init__(self, items):\n        self.items = items\n    def slot(self, number):\n        return next((item for item in self.items if item.slot == number), None)\n    def count(self):\n        return len(self.items)\n";
    let after = before.replace("    def slot(self, number):\n        return next((item for item in self.items if item.slot == number), None)\n", "");
    let output = a3_review_hook(
        |root| std::fs::write(root.join("shop/board.py"), &after).unwrap(),
        &[
            ("shop/__init__.py", ""),
            ("shop/board.py", before),
            (
                "shop/report.py",
                "from shop.board import Board\n\ndef total(board: Board):\n    return sum(item.slot for item in board.items)\n",
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
