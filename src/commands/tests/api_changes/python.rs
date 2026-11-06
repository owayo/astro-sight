//! Python の API 差分検出テスト。

#[test]
fn python_changed_caller_must_supply_new_required_argument_to_close_api_mod() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };

    for (label, call, should_close) in [
        ("unrelated edit", "render('new')", false),
        ("positional argument", "render('new', 'blue')", true),
        ("keyword argument", "render('new', color='blue')", true),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("lib.py", "def render(name: str) -> str:\n    return name\n"),
                (
                    "app.py",
                    "from lib import render\n\nvalue = render('old')\n",
                ),
            ],
            "initial",
        );
        std::fs::write(
            repo.join("lib.py"),
            "def render(name: str, color: str) -> str:\n    return name + color\n",
        )
        .expect("write definition");
        std::fs::write(
            repo.join("app.py"),
            format!("from lib import render\n\nvalue = {call}\n"),
        )
        .expect("write caller");

        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|change| change.name == "render"),
            should_close,
            "{label}: {changes:?}"
        );
        assert_eq!(
            changes
                .modified
                .iter()
                .any(|change| change.name == "render"),
            !should_close,
            "{label}: {changes:?}"
        );
    }
}

#[test]
fn python_changed_call_respects_keyword_only_positional_only_and_splats() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };

    for (label, definition, call, should_close) in [
        (
            "keyword-only supplied",
            "def render(name: str, *, color: str) -> str:\n    return name + color\n",
            "render('new', color='blue')",
            true,
        ),
        (
            "keyword-only missing",
            "def render(name: str, *, color: str) -> str:\n    return name + color\n",
            "render('new')",
            false,
        ),
        (
            "keyword-only passed positionally",
            "def render(name: str, *, color: str) -> str:\n    return name + color\n",
            "render('new', 'blue')",
            false,
        ),
        (
            "positional-only passed by keyword",
            "def render(name: str, color: str, /) -> str:\n    return name + color\n",
            "render('new', color='blue')",
            false,
        ),
        (
            "positional-only supplied",
            "def render(name: str, color: str, /) -> str:\n    return name + color\n",
            "render('new', 'blue')",
            true,
        ),
        (
            "positional-only name can enter kwargs",
            "def render(name: str, /, **options: str) -> str:\n    return name\n",
            "render('new', name='alias')",
            true,
        ),
        (
            "positional-only still requires positional value",
            "def render(name: str, /, **options: str) -> str:\n    return name\n",
            "render(name='alias')",
            false,
        ),
        (
            "splat is uncertain",
            "def render(name: str, color: str) -> str:\n    return name + color\n",
            "render('new', *args)",
            false,
        ),
        (
            "positional after keyword",
            "def render(name: str, color: str) -> str:\n    return name + color\n",
            "render(color='blue', 'new')",
            false,
        ),
        (
            "variadic positional accepts surplus",
            "def render(name: str, color: str, *rest) -> str:\n    return name + color\n",
            "render('new', 'blue', 'extra')",
            true,
        ),
        (
            "variadic keyword accepts surplus",
            "def render(name: str, color: str, **options) -> str:\n    return name + color\n",
            "render('new', 'blue', mood='bright')",
            true,
        ),
        (
            "typed variadics accept surplus",
            "def render(name: str, color: str, *rest: str, **options: str) -> str:\n    return name + color\n",
            "render('new', 'blue', 'extra', mood='bright')",
            true,
        ),
        (
            "typed variadics still require color",
            "def render(name: str, color: str, *rest: str, **options: str) -> str:\n    return name + color\n",
            "render('new', mood='bright')",
            false,
        ),
        (
            "decorated function is uncertain",
            "@register\ndef render(name: str, color: str) -> str:\n    return name + color\n",
            "render('new', 'blue')",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("lib.py", "def render(name: str) -> str:\n    return name\n"),
                (
                    "app.py",
                    "from lib import render\n\nvalue = render('old')\n",
                ),
            ],
            "initial",
        );
        std::fs::write(repo.join("lib.py"), definition).expect("write definition");
        std::fs::write(
            repo.join("app.py"),
            format!("from lib import render\n\nvalue = {call}\n"),
        )
        .expect("write caller");

        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|change| change.name == "render"),
            should_close,
            "{label}: {changes:?}"
        );
        assert_eq!(
            changes
                .modified
                .iter()
                .any(|change| change.name == "render"),
            !should_close,
            "{label}: {changes:?}"
        );
    }
}

#[test]
fn python_module_qualified_changed_call_keeps_valid_and_invalid_arity_distinct() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };

    for (call, should_close) in [
        ("lib.render('new', 'blue')", true),
        ("lib.render('new')", false),
        ("pkg.lib.render('new', 'blue')", true),
        ("pkg.lib.render('new')", false),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        let deep = call.starts_with("pkg.");
        let target = if deep { "pkg/lib.py" } else { "lib.py" };
        let imported = if deep { "import pkg.lib" } else { "import lib" };
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (target, "def render(name: str) -> str:\n    return name\n"),
                (
                    "app.py",
                    if deep {
                        "import pkg.lib\n\nvalue = pkg.lib.render('old')\n"
                    } else {
                        "import lib\n\nvalue = lib.render('old')\n"
                    },
                ),
            ],
            "initial",
        );
        std::fs::write(
            repo.join(target),
            "def render(name: str, color: str) -> str:\n    return name + color\n",
        )
        .expect("write definition");
        std::fs::write(
            repo.join("app.py"),
            format!("{imported}\n\nvalue = {call}\n"),
        )
        .expect("write caller");

        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|change| change.name == "render"),
            should_close,
            "{call}: {changes:?}"
        );
        assert_eq!(
            changes
                .modified
                .iter()
                .any(|change| change.name == "render"),
            !should_close,
            "{call}: {changes:?}"
        );
    }
}

#[test]
fn python_rebinding_and_function_values_do_not_close_changed_calls() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };

    for (label, suffix, caller) in [
        (
            "rebound with partial",
            "\nfrom functools import partial\nrender = partial(render, color='blue')\n",
            "value = render('new', 'blue')\n",
        ),
        (
            "rebound through conditional import",
            "\ntry:\n    from speedups import render\nexcept ImportError:\n    pass\n",
            "value = render('new', 'blue')\n",
        ),
        (
            "rebound through import alias",
            "\nfrom speedups import fast as render\n",
            "value = render('new', 'blue')\n",
        ),
        (
            "rebound through wildcard import",
            "\nfrom speedups import *\n",
            "value = render('new', 'blue')\n",
        ),
        (
            "function value",
            "",
            "value = render('new', 'blue')\nhandlers = [render]\n",
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("lib.py", "def render(name: str) -> str:\n    return name\n"),
                (
                    "app.py",
                    "from lib import render\n\nvalue = render('old')\n",
                ),
            ],
            "initial",
        );
        std::fs::write(
            repo.join("lib.py"),
            format!("def render(name: str, color: str) -> str:\n    return name + color\n{suffix}"),
        )
        .expect("write definition");
        std::fs::write(
            repo.join("app.py"),
            format!("from lib import render\n\n{caller}"),
        )
        .expect("write caller");

        let changes = detect_api_changes_from_worktree(repo);
        assert!(
            changes
                .modified
                .iter()
                .any(|change| change.name == "render"),
            "{label}: {changes:?}"
        );
        assert!(
            !changes
                .modified_closed_in_diff
                .iter()
                .any(|change| change.name == "render"),
            "{label}: {changes:?}"
        );
    }
}

#[test]
fn python_same_name_methods_close_only_when_all_definitions_and_callers_change() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };

    for (label, update_fake, update_summary, malformed_fake, should_close) in [
        ("all updated", true, true, false, true),
        ("unchanged fake definition", false, true, false, false),
        ("unchanged caller", true, false, false, false),
        ("malformed fake file", true, true, true, false),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("shop/__init__.py", ""),
                (
                    "shop/catalog.py",
                    "class Catalog:\n    def price(self, sku: str) -> int:\n        return len(sku)\n",
                ),
                (
                    "shop/report.py",
                    "from typing import Protocol\n\nclass PriceSource(Protocol):\n    def price(self, sku: str) -> int: ...\n\ndef summary(source: PriceSource) -> int:\n    return source.price('a')\n",
                ),
                (
                    "tests/test_report.py",
                    "from shop.report import summary\n\nclass FakeSource:\n    def price(self, sku: str) -> int:\n        return 1\n\ndef test_summary():\n    assert summary(FakeSource()) == 1\n",
                ),
                (
                    "tests/test_catalog.py",
                    "from shop.catalog import Catalog\n\ndef test_price():\n    assert Catalog().price('ab') == 2\n",
                ),
            ],
            "initial",
        );
        std::fs::write(
            repo.join("shop/catalog.py"),
            "class Catalog:\n    def price(self, sku: str, currency: str) -> int:\n        return len(sku) + len(currency)\n",
        )
        .expect("write catalog");
        std::fs::write(
            repo.join("shop/report.py"),
            if update_summary {
                "from typing import Protocol\n\nclass PriceSource(Protocol):\n    def price(self, sku: str, currency: str) -> int: ...\n\ndef summary(source: PriceSource) -> int:\n    return source.price('a', 'usd')\n"
            } else {
                "from typing import Protocol\n\nclass PriceSource(Protocol):\n    def price(self, sku: str, currency: str) -> int: ...\n\ndef summary(source: PriceSource) -> int:\n    return source.price('a')\n"
            },
        )
        .expect("write report");
        if update_fake {
            std::fs::write(
                repo.join("tests/test_report.py"),
                if malformed_fake {
                    "from shop.report import summary\n\nclass FakeSource:\n    def price(self, sku: str, currency: str) -> int:\n        return 1\n\ndef test_summary():\n    assert summary(FakeSource()) == 1\n\ndef broken(\n"
                } else {
                    "from shop.report import summary\n\nclass FakeSource:\n    def price(self, sku: str, currency: str) -> int:\n        return 1\n\ndef test_summary():\n    assert summary(FakeSource()) == 1\n"
                },
            )
            .expect("write fake");
        }
        std::fs::write(
            repo.join("tests/test_catalog.py"),
            "from shop.catalog import Catalog\n\ndef test_price():\n    assert Catalog().price('ab', '') == 2\n",
        )
        .expect("write test");

        let changes = detect_api_changes_from_worktree(repo);
        for name in ["Catalog.price", "PriceSource.price"] {
            assert_eq!(
                changes
                    .modified_closed_in_diff
                    .iter()
                    .any(|change| change.name == name),
                should_close,
                "{label} / {name}: {changes:?}"
            );
            assert_eq!(
                changes.modified.iter().any(|change| change.name == name),
                !should_close,
                "{label} / {name}: {changes:?}"
            );
        }
    }
}

#[test]
fn python_changed_method_call_requires_new_argument_before_closing() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };

    for (label, caller, should_close) in [
        (
            "unrelated caller edit",
            "from pkg.service import Service\n\ndef run() -> int:\n    return Service().price(1) + 0\n",
            false,
        ),
        (
            "required argument supplied",
            "from pkg.service import Service\n\ndef run() -> int:\n    return Service().price(1, 2)\n",
            true,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/service.py",
                    "class Service:\n    def price(self, amount: int) -> int:\n        return amount\n",
                ),
                (
                    "pkg/caller.py",
                    "from pkg.service import Service\n\ndef run() -> int:\n    return Service().price(1)\n",
                ),
            ],
            "base",
        );
        std::fs::write(
            repo.join("pkg/service.py"),
            "class Service:\n    def price(self, amount: int, tax: int) -> int:\n        return amount + tax\n",
        )
        .expect("write service");
        std::fs::write(repo.join("pkg/caller.py"), caller).expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|change| change.name == "Service.price"),
            should_close,
            "{label}: {changes:?}"
        );
        assert_eq!(
            changes
                .modified
                .iter()
                .any(|change| change.name == "Service.price"),
            !should_close,
            "{label}: {changes:?}"
        );
    }
}

#[test]
fn python_method_receiver_binding_controls_argument_check() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (label, before, after, should_close) in [
        (
            "bound missing",
            "Service().price(1)",
            "Service().price(1) + 0",
            false,
        ),
        (
            "bound updated",
            "Service().price(1)",
            "Service().price(1, 2)",
            true,
        ),
        (
            "local missing",
            "obj = Service()\n    return obj.price(1)",
            "obj = Service()\n    return obj.price(1) + 0",
            false,
        ),
        (
            "local updated",
            "obj = Service()\n    return obj.price(1)",
            "obj = Service()\n    return obj.price(1, 2)",
            true,
        ),
        (
            "unbound missing",
            "Service.price(Service(), 1)",
            "Service.price(Service(), 1) + 0",
            false,
        ),
        (
            "unbound updated",
            "Service.price(Service(), 1)",
            "Service.price(Service(), 1, 2)",
            true,
        ),
        (
            "unknown alias",
            "Service.price(Service(), 1)",
            "Alias = Service\n    return Alias.price(Service(), 1, 2)",
            false,
        ),
        (
            "comprehension shadows class",
            "Service.price(Service(), 1)",
            "[Service.price(1, 2, 3) for Service in []]",
            false,
        ),
        (
            "lambda shadows class",
            "Service.price(Service(), 1)",
            "(lambda Service: Service.price(1, 2, 3))(Service())",
            false,
        ),
        (
            "rebound local receiver",
            "obj = Service()\n    return obj.price(1)",
            "obj = Service()\n    obj = Service\n    return obj.price(1, 2)",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let before_call = if before.contains("\n") {
            before.to_owned()
        } else {
            format!("return {before}")
        };
        let after_call = if after.contains("\n") {
            after.to_owned()
        } else {
            format!("return {after}")
        };
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/service.py",
                    "class Service:\n    def price(self, amount: int) -> int:\n        return amount\n",
                ),
                (
                    "pkg/caller.py",
                    &format!("from pkg.service import Service\n\ndef run():\n    {before_call}\n"),
                ),
            ],
            "base",
        );
        std::fs::write(repo.join("pkg/service.py"), "class Service:\n    def price(self, amount: int, tax: int) -> int:\n        return amount + tax\n").expect("write service");
        std::fs::write(
            repo.join("pkg/caller.py"),
            format!("from pkg.service import Service\n\ndef run():\n    {after_call}\n"),
        )
        .expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Service.price"),
            should_close,
            "{label}: {changes:?}"
        );
        assert_eq!(
            changes.modified.iter().any(|c| c.name == "Service.price"),
            !should_close,
            "{label}: {changes:?}"
        );
    }
}

#[test]
fn python_method_decorators_use_their_binding_contract() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (label, decorator, first, before, after, should_close) in [
        (
            "class missing",
            "@classmethod",
            "cls",
            "Service.price(1)",
            "Service.price(1) + 0",
            false,
        ),
        (
            "class updated",
            "@classmethod",
            "cls",
            "Service.price(1)",
            "Service.price(1, 2)",
            true,
        ),
        (
            "static missing",
            "@staticmethod",
            "amount",
            "Service.price(1)",
            "Service.price(1) + 0",
            false,
        ),
        (
            "static updated",
            "@staticmethod",
            "amount",
            "Service.price(1)",
            "Service.price(1, 2)",
            true,
        ),
        (
            "unknown decorator",
            "@wrapper",
            "self",
            "Service.price(Service(), 1)",
            "Service.price(Service(), 1, 2)",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let old_params = if first == "amount" {
            "amount: int"
        } else {
            "cls, amount: int"
        };
        let new_params = if first == "amount" {
            "amount: int, tax: int"
        } else {
            "cls, amount: int, tax: int"
        };
        let old_service = format!(
            "class Service:\n    {decorator}\n    def price({old_params}) -> int:\n        return amount\n"
        );
        let new_service = format!(
            "class Service:\n    {decorator}\n    def price({new_params}) -> int:\n        return amount + tax\n"
        );
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                ("pkg/service.py", &old_service),
                (
                    "pkg/caller.py",
                    &format!(
                        "from pkg.service import Service\n\ndef run():\n    return {before}\n"
                    ),
                ),
            ],
            "base",
        );
        std::fs::write(repo.join("pkg/service.py"), new_service).expect("write service");
        std::fs::write(
            repo.join("pkg/caller.py"),
            format!("from pkg.service import Service\n\ndef run():\n    return {after}\n"),
        )
        .expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Service.price"),
            should_close,
            "{label}: {changes:?}"
        );
        assert_eq!(
            changes.modified.iter().any(|c| c.name == "Service.price"),
            !should_close,
            "{label}: {changes:?}"
        );
    }
}

#[test]
fn python_self_method_call_checks_the_new_parameters() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (caller, should_close) in [
        ("return self.price(1) + 0", false),
        ("return self.price(1, 2)", true),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/service.py",
                    "class Service:\n    def price(self, amount):\n        return amount\n\n    def run(self):\n        return self.price(1)\n",
                ),
                (
                    "pkg/caller.py",
                    "from pkg.service import Service\n\ndef use():\n    return Service().price(1)\n",
                ),
            ],
            "base",
        );
        std::fs::write(repo.join("pkg/service.py"), format!("class Service:\n    def price(self, amount, tax):\n        return amount + tax\n\n    def run(self):\n        {caller}\n")).expect("write service");
        std::fs::write(
            repo.join("pkg/caller.py"),
            "from pkg.service import Service\n\ndef use():\n    return Service().price(1, 2)\n",
        )
        .expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Service.price"),
            should_close,
            "{caller}: {changes:?}"
        );
        assert_eq!(
            changes.modified.iter().any(|c| c.name == "Service.price"),
            !should_close,
            "{caller}: {changes:?}"
        );
    }
}

#[test]
fn python_same_name_methods_check_their_own_argument_contracts() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (catalog_call, ledger_call, catalog_closed, ledger_closed) in [
        (
            "Catalog().price('a', 'u')",
            "Ledger().price('a', 'u', 'r')",
            true,
            true,
        ),
        (
            "Catalog().price('a', 'u')",
            "Ledger().price('a', 'r') + 0",
            true,
            false,
        ),
        ("0", "Ledger().price('a', 'u', 'r')", false, true),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/catalog.py",
                    "class Catalog:\n    def price(self, sku):\n        return len(sku)\n",
                ),
                (
                    "pkg/ledger.py",
                    "class Ledger:\n    def price(self, sku, region):\n        return len(sku) + len(region)\n",
                ),
                (
                    "pkg/caller.py",
                    "from pkg.catalog import Catalog\nfrom pkg.ledger import Ledger\n\ndef total():\n    return Catalog().price('a') + Ledger().price('a', 'r')\n",
                ),
            ],
            "base",
        );
        std::fs::write(repo.join("pkg/catalog.py"), "class Catalog:\n    def price(self, sku, currency):\n        return len(sku) + len(currency)\n").expect("write catalog");
        std::fs::write(repo.join("pkg/ledger.py"), "class Ledger:\n    def price(self, sku, currency, region):\n        return len(sku) + len(currency) + len(region)\n").expect("write ledger");
        std::fs::write(repo.join("pkg/caller.py"), format!("from pkg.catalog import Catalog\nfrom pkg.ledger import Ledger\n\ndef total():\n    return {catalog_call} + {ledger_call}\n")).expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        for (name, expected) in [
            ("Catalog.price", catalog_closed),
            ("Ledger.price", ledger_closed),
        ] {
            assert_eq!(
                changes
                    .modified_closed_in_diff
                    .iter()
                    .any(|c| c.name == name),
                expected,
                "{name}: {changes:?}"
            );
            assert_eq!(
                changes.modified.iter().any(|c| c.name == name),
                !expected,
                "{name}: {changes:?}"
            );
        }
    }
}

#[test]
fn python_unchanged_other_owner_is_excluded_only_with_exact_module_and_receiver() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (label, other_class, other_call, duplicate_root, expected) in [
        (
            "direct",
            "class Ledger:\n    def price(self, amount):\n        return amount\n",
            "Ledger().price(1)",
            false,
            true,
        ),
        (
            "one hop",
            "class Ledger:\n    def price(self, amount):\n        return amount\n",
            "item.price(1)",
            false,
            true,
        ),
        (
            "unknown receiver",
            "class Ledger:\n    def price(self, amount):\n        return amount\n",
            "unknown().price(1)",
            false,
            false,
        ),
        (
            "inherited",
            "class Base:\n    def price(self, amount):\n        return amount\n\nclass Ledger(Base):\n    pass\n",
            "Ledger().price(1)",
            false,
            false,
        ),
        (
            "duplicate root",
            "class Ledger:\n    def price(self, amount):\n        return amount\n",
            "Ledger().price(1)",
            true,
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let foreign = format!(
            "from pkg.other import Ledger\n\ndef other():\n    item = Ledger()\n    return {other_call}\n"
        );
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/catalog.py",
                    "class Catalog:\n    def price(self, amount):\n        return amount\n",
                ),
                ("pkg/other.py", other_class),
                (
                    "pkg/caller.py",
                    "from pkg.catalog import Catalog\n\ndef use():\n    item = Catalog()\n    return item.price(1)\n",
                ),
                ("pkg/foreign.py", &foreign),
            ],
            "base",
        );
        if duplicate_root {
            std::fs::create_dir_all(repo.join("src/pkg")).unwrap();
            std::fs::write(repo.join("src/pkg/__init__.py"), "").unwrap();
            std::fs::write(repo.join("src/pkg/other.py"), other_class).unwrap();
        }
        std::fs::write(
            repo.join("pkg/catalog.py"),
            "class Catalog:\n    def price(self, amount, tax):\n        return amount + tax\n",
        )
        .unwrap();
        std::fs::write(repo.join("pkg/caller.py"), "from pkg.catalog import Catalog\n\ndef use():\n    item = Catalog()\n    return item.price(1, 2)\n").unwrap();
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Catalog.price"),
            expected,
            "{label}: {changes:?}"
        );
        assert_eq!(
            changes.modified.iter().any(|c| c.name == "Catalog.price"),
            !expected,
            "{label}: {changes:?}"
        );
    }
}

#[test]
fn python_same_class_name_in_another_file_is_not_the_target_owner() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for target_used in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/catalog.py",
                    "class Catalog:\n    def price(self, amount):\n        return amount\n",
                ),
                (
                    "pkg/other.py",
                    "class Catalog:\n    def price(self, amount):\n        return amount\n",
                ),
                (
                    "pkg/foreign.py",
                    "from pkg.other import Catalog\n\ndef use_other():\n    return Catalog().price(1)\n",
                ),
                (
                    "pkg/caller.py",
                    if target_used {
                        "from pkg.catalog import Catalog\n\ndef use():\n    return Catalog().price(1)\n"
                    } else {
                        "def use():\n    return 0\n"
                    },
                ),
            ],
            "base",
        );
        std::fs::write(
            repo.join("pkg/catalog.py"),
            "class Catalog:\n    def price(self, amount, tax):\n        return amount + tax\n",
        )
        .unwrap();
        if target_used {
            std::fs::write(
                repo.join("pkg/caller.py"),
                "from pkg.catalog import Catalog\n\ndef use():\n    return Catalog().price(1, 2)\n",
            )
            .unwrap();
        }
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Catalog.price"),
            target_used,
            "{changes:?}"
        );
        assert_eq!(
            changes.modified.iter().any(|c| c.name == "Catalog.price"),
            !target_used,
            "{changes:?}"
        );
    }
}

#[test]
fn python_same_file_other_method_definition_has_a_distinct_class_owner() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let other = "\nclass Ledger:\n    def price(self, amount):\n        return amount\n";
    git_commit_files(
        repo,
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/catalog.py",
                &format!(
                    "class Catalog:\n    def price(self, amount):\n        return amount\n{other}"
                ),
            ),
            (
                "pkg/caller.py",
                "from pkg.catalog import Catalog, Ledger\n\ndef use():\n    return Catalog().price(1) + Ledger().price(1)\n",
            ),
        ],
        "base",
    );
    std::fs::write(repo.join("pkg/catalog.py"), format!("class Catalog:\n    def price(self, amount, tax):\n        return amount + tax\n{other}")).unwrap();
    std::fs::write(repo.join("pkg/caller.py"), "from pkg.catalog import Catalog, Ledger\n\ndef use():\n    return Catalog().price(1, 2) + Ledger().price(1)\n").unwrap();
    let changes = detect_api_changes_from_worktree(repo);
    assert!(
        changes
            .modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Catalog.price"),
        "{changes:?}"
    );
}

#[test]
fn python_bound_receiver_keyword_is_checked_before_kwargs() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (separator, keyword, should_close) in [
        ("", "self=Service()", false),
        ("", "extra=1", true),
        ("/, ", "self=Service()", true),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/service.py",
                    &format!(
                        "class Service:\n    def price(self, {separator}amount, **kwargs):\n        return amount\n"
                    ),
                ),
                (
                    "pkg/caller.py",
                    "from pkg.service import Service\n\ndef use():\n    return Service().price(1)\n",
                ),
            ],
            "base",
        );
        std::fs::write(repo.join("pkg/service.py"), format!("class Service:\n    def price(self, {separator}amount, tax, **kwargs):\n        return amount + tax\n")).expect("write service");
        std::fs::write(repo.join("pkg/caller.py"), format!("from pkg.service import Service\n\ndef use():\n    return Service().price(1, 2, {keyword})\n")).expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Service.price"),
            should_close,
            "{separator} / {keyword}: {changes:?}"
        );
        assert_eq!(
            changes.modified.iter().any(|c| c.name == "Service.price"),
            !should_close,
            "{separator} / {keyword}: {changes:?}"
        );
    }
}

#[test]
fn python_self_receiver_does_not_exclude_a_method_selected_by_mro() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/service.py",
                "class Service:\n    def price(self, a, b):\n        return a + b\n",
            ),
            (
                "pkg/other.py",
                "class Other:\n    def price(self, a):\n        return a\n\n    def run(self):\n        return self.price(1)\n",
            ),
            (
                "pkg/caller.py",
                "from pkg.service import Service\nfrom pkg.other import Other\n\nclass Combined(Service, Other):\n    pass\n\ndef use():\n    return Service().price(1, 2) + Combined().run()\n",
            ),
        ],
        "base",
    );
    std::fs::write(
        repo.join("pkg/service.py"),
        "class Service:\n    def price(self, a, b, c):\n        return a + b + c\n",
    )
    .expect("write service");
    std::fs::write(repo.join("pkg/other.py"), "class Other:\n    def price(self, a, b):\n        return a + b\n\n    def run(self):\n        return self.price(1, 2)\n").expect("write other");
    std::fs::write(repo.join("pkg/caller.py"), "from pkg.service import Service\nfrom pkg.other import Other\n\nclass Combined(Service, Other):\n    pass\n\ndef use():\n    return Service().price(1, 2, 3) + Combined().run()\n").expect("write caller");
    let changes = detect_api_changes_from_worktree(repo);
    assert!(
        changes.modified.iter().any(|c| c.name == "Service.price"),
        "{changes:?}"
    );
    assert!(
        !changes
            .modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Service.price"),
        "{changes:?}"
    );
}

#[test]
fn python_class_change_keeps_existing_closed_in_diff_behavior() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/service.py",
                "class Base:\n    pass\n\nclass Service:\n    pass\n",
            ),
            (
                "pkg/caller.py",
                "from pkg.service import Service\n\ndef run():\n    return Service()\n",
            ),
        ],
        "base",
    );
    std::fs::write(
        repo.join("pkg/service.py"),
        "class Base:\n    pass\n\nclass Service(Base):\n    pass\n",
    )
    .expect("write service");
    std::fs::write(
        repo.join("pkg/caller.py"),
        "from pkg.service import Service\n\ndef run():\n    return Service()  # updated\n",
    )
    .expect("write caller");
    let changes = detect_api_changes_from_worktree(repo);
    assert!(
        changes
            .modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Service"),
        "{changes:?}"
    );
    assert!(
        !changes.modified.iter().any(|c| c.name == "Service"),
        "{changes:?}"
    );
}

#[test]
fn python_method_import_resolution_handles_multiple_names_and_src_layout() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (prefix, import_line) in [
        ("pkg", "from pkg.service import Service, Other"),
        ("src/pkg", "from pkg.service import Service"),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let init = format!("{prefix}/__init__.py");
        let service = format!("{prefix}/service.py");
        let caller = format!("{prefix}/caller.py");
        git_commit_files(
            repo,
            &[
                (&init, ""),
                (
                    &service,
                    "class Service:\n    def price(self, amount):\n        return amount\n\nclass Other:\n    pass\n",
                ),
                (
                    &caller,
                    &format!("{import_line}\n\ndef use():\n    return Service().price(1)\n"),
                ),
            ],
            "base",
        );
        std::fs::write(repo.join(&service), "class Service:\n    def price(self, amount, tax):\n        return amount + tax\n\nclass Other:\n    pass\n").expect("write service");
        std::fs::write(
            repo.join(&caller),
            format!("{import_line}\n\ndef use():\n    return Service().price(1, 2)\n"),
        )
        .expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        assert!(
            changes
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Service.price"),
            "{prefix}: {changes:?}"
        );
        assert!(
            !changes.modified.iter().any(|c| c.name == "Service.price"),
            "{prefix}: {changes:?}"
        );
    }
}

#[test]
fn python_same_name_test_fake_does_not_block_updated_method_and_callers() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("shop/__init__.py", ""),
            (
                "shop/catalog.py",
                "class Catalog:\n    def price(self, sku: str) -> int:\n        return len(sku)\n",
            ),
            (
                "shop/report.py",
                "from shop.catalog import Catalog\n\ndef summary(catalog: Catalog) -> int:\n    return catalog.price('a')\n",
            ),
            (
                "tests/test_fake.py",
                "class FakeCatalog:\n    def price(self, sku: str) -> int:\n        return 1\n\ndef test_fake():\n    assert FakeCatalog().price('a') == 1\n",
            ),
        ],
        "initial",
    );
    std::fs::write(
        repo.join("shop/catalog.py"),
        "class Catalog:\n    def price(self, sku: str, currency: str) -> int:\n        return len(sku) + len(currency)\n",
    )
    .expect("write catalog");
    std::fs::write(
        repo.join("shop/report.py"),
        "from shop.catalog import Catalog\n\ndef summary(catalog: Catalog) -> int:\n    return catalog.price('a', 'usd')\n",
    )
    .expect("write report");
    std::fs::write(
        repo.join("tests/test_fake.py"),
        "class FakeCatalog:\n    def price(self, sku: str, currency: str) -> int:\n        return 1\n\ndef test_fake():\n    assert FakeCatalog().price('a', 'usd') == 1\n",
    )
    .expect("write fake");

    let changes = detect_api_changes_from_worktree(repo);
    assert!(
        changes
            .modified_closed_in_diff
            .iter()
            .any(|change| change.name == "Catalog.price"),
        "updated fake and caller should close the changed method: {changes:?}"
    );
}

#[test]
fn detect_api_changes_python_lambda_binding_replaces_callable_not_name() {
    use crate::commands::tests::common::{
        detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
    };
    for (before, after) in [
        (
            "def normalize(text):\n    return text.strip()\n",
            "normalize = lambda text: text.strip()\n",
        ),
        (
            "normalize = lambda text: text.strip()\n",
            "def normalize(text):\n    return text.strip()\n",
        ),
        (
            "for item in [1]:\n    def normalize(text):\n        return text.strip()\n",
            "for item in [1]:\n    normalize = lambda text: text.strip()\n",
        ),
        (
            "def normalize(text):\n    return text.strip()\n",
            "normalize = lambda text, mode: text.strip()\n",
        ),
        (
            "normalize = lambda text: 'a  b'\n",
            "normalize = lambda text: 'a b'\n",
        ),
        (
            "normalize = lambda text: (\n    text.strip()\n)\n",
            "normalize = lambda text: (\n    text.lower()\n)\n",
        ),
        (
            "class Handler:\n    def normalize(self, text):\n        return text.strip()\n",
            "class Handler:\n    normalize = lambda self, text: text.strip()\n",
        ),
        (
            "class Handler:\n    normalize = lambda self, text: text.strip()\n",
            "class Handler:\n    def normalize(self, text):\n        return text.strip()\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let (name, consumer) = if before.starts_with("class Handler:") {
            (
                "Handler.normalize",
                "from core import Handler\nprint(Handler().normalize(' A '))\n",
            )
        } else {
            (
                "normalize",
                "from core import normalize\nprint(normalize(' A '))\n",
            )
        };
        git_commit_files(
            repo,
            &[("core.py", before), ("consumer.py", consumer)],
            "base",
        );
        std::fs::write(repo.join("core.py"), after).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert!(
            api.removed.is_empty() && api.removed_dead.is_empty(),
            "{before} -> {after}: {api:?}"
        );
        assert!(
            api.modified
                .iter()
                .filter(|change| change.name == name)
                .count()
                == 1
                && !api.added.iter().any(|change| change.name == name),
            "メタデータ・引数・本体の変化を同値扱いしない: {api:?}"
        );
    }
    // callable でない置換と本当の削除は削除として残す。
    for before in [
        "def normalize(text):\n    return text.strip()\n",
        "normalize = lambda text: text.strip()\n",
    ] {
        for after in [
            "normalize = 42\n",
            "",
            "normalize = other = lambda text: text.strip()\n",
            "normalize = (lambda text: text.strip())\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            init_git_repo_for_test(repo);
            git_commit_files(
                repo,
                &[
                    ("core.py", before),
                    (
                        "consumer.py",
                        "from core import normalize\nprint(normalize(' A '))\n",
                    ),
                ],
                "base",
            );
            std::fs::write(repo.join("core.py"), after).unwrap();
            let api = detect_api_changes_from_worktree(repo);
            assert!(
                api.removed.iter().any(|change| change.name == "normalize"),
                "{api:?}"
            );
        }
    }
    // 整形だけでは callable の契約変更を作らない。
    for (before, after) in [
        (
            "normalize = lambda text: text.strip()\n",
            "normalize = lambda  text :text.strip()  # note\n",
        ),
        (
            "normalize = lambda text: (\n    text.strip()\n)\n",
            "normalize = lambda text: (text.strip())\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("core.py", before),
                (
                    "consumer.py",
                    "from core import normalize\nprint(normalize(' A '))\n",
                ),
            ],
            "base",
        );
        std::fs::write(repo.join("core.py"), after).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert!(
            api.modified.is_empty()
                && api.added.is_empty()
                && api.removed.is_empty()
                && api.removed_dead.is_empty(),
            "{api:?}"
        );
    }
}

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
#[allow(unused_imports)]
use std::collections::HashSet;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::Cursor;
#[allow(unused_imports)]
use std::process::Command;

#[test]
fn extract_python_class_fields_collects_typed_annotations_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let py = "\
from dataclasses import dataclass


@dataclass
class A:
    x: int
    y: str = \"default\"
    untyped = 1


class B:
    z: float
";
    fs::write(dir.path().join("m.py"), py).expect("write");

    let a_fields = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "A");
    assert!(
        a_fields.contains("x"),
        "typed annotation は採取される: {a_fields:?}"
    );
    assert!(
        a_fields.contains("y"),
        "default 値付き typed annotation も採取される: {a_fields:?}"
    );
    assert!(
        !a_fields.contains("untyped"),
        "type annotation が無い代入は採取しない: {a_fields:?}"
    );

    let b_fields = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "B");
    assert!(
        b_fields.contains("z"),
        "@dataclass でないクラスでも採取する: {b_fields:?}"
    );

    let none = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "Missing");
    assert!(none.is_empty(), "存在しないクラス名は空集合: {none:?}");
}

/// Python で同一ファイル内から呼ばれている新規 public 関数は api.add に出ない。
#[test]
fn detect_api_changes_python_internally_called_function_is_not_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "def main():\n    print(\"hi\")\n";
    git_commit_files(repo, &[("svc.py", before)], "initial");

    // helper を追加し、main から呼ぶ
    let after = "def helper() -> str:\n    return \"x\"\n\n\
def main():\n    helper()\n    print(\"hi\")\n";
    fs::write(repo.join("svc.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "svc.py".to_string(),
        new_path: "svc.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.contains(&"helper"),
        "同一ファイル内で呼ばれている Python 関数は api.add に出してはならない。got: {added:?}"
    );
}

/// Python CLI スクリプト（同一ファイル内でのみ呼ばれる関数）のシグネチャ変更は
/// caller が同じ diff 内で追随できるため api.mod に出さない。
/// (レポート 2026-04-22-closed-in-diff-signature-change-noise.md の再現)
#[test]
fn detect_api_changes_python_cli_signature_change_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
def run_osv_scanner(path: str) -> int:
    return 0


def scan_worktree(path: str) -> int:
    rc = run_osv_scanner(path)
    return rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
    git_commit_files(repo, &[("osv_scan.py", before)], "initial");

    // run_osv_scanner の戻り値型を int -> tuple[int, float] に変更。
    // caller (scan_worktree) も同じ diff 内で追随する。
    let after = "\
def run_osv_scanner(path: str) -> tuple[int, float]:
    return (0, 0.0)


def scan_worktree(path: str) -> int:
    _rc, _elapsed = run_osv_scanner(path)
    return _rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
    fs::write(repo.join("osv_scan.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "osv_scan.py".to_string(),
        new_path: "osv_scan.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 11,
            new_start: 1,
            new_count: 11,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"run_osv_scanner"),
        "同一ファイル内でのみ呼ばれる関数のシグネチャ変更は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// `detect_python_property_to_field` は old_path が Python で、base 側が property の場合だけ
/// 置き換え先を返す (他言語の `Container.member` 削除や、素のメソッドの置き換えを
/// informational に降格しない)。別ファイルの同名クラスを根拠にしないことは
/// `detect_api_changes_python_property_removed_with_field_in_other_file_stays_removed` で固定する。
#[test]
fn detect_python_property_to_field_requires_python_old_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "m.py",
            "class Container:\n    @property\n    def member(self) -> int:\n        return 1\n\n    def method(self) -> int:\n        return 2\n",
        )],
        "base",
    );
    fs::write(
        repo.join("m.py"),
        "from dataclasses import dataclass\n@dataclass\nclass Container:\n    member: int\n    method: int\n",
    )
    .expect("write");
    let dir_str = repo.to_str().expect("utf-8 path");

    assert_eq!(
        detect_python_property_to_field(dir_str, "HEAD", "m.py", "m.py", "Container.member"),
        Some("m.py".to_string()),
        "Python の property を同じファイルのフィールドに置き換えたなら検出する"
    );
    assert_eq!(
        detect_python_property_to_field(dir_str, "HEAD", "m.ts", "m.py", "Container.member"),
        None,
        "Python 以外の old_path は言語ガードで対象外"
    );
    assert_eq!(
        detect_python_property_to_field(dir_str, "HEAD", "m.py", "m.py", "Container.method"),
        None,
        "素のメソッドをフィールドに置き換えると `obj.method()` が壊れるので対象外"
    );
}

/// `models.py` を `before` から `after` に書き換えたときの API 差分。`app.py` は変更しない。
fn python_models_change(before: &str, after: &str) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("models.py", before),
            (
                "app.py",
                "from models import User\n\n\ndef show(u: User):\n    return u.name(), User.build(1)\n",
            ),
        ],
        "base",
    );
    fs::write(repo.join("models.py"), after).expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "models.py".to_string(),
        new_path: "models.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 20,
            new_start: 1,
            new_count: 20,
        }],
        deleted_old_source: None,
    }];
    detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files)
}

/// `def` 行が変わらなくても、束縛を変えるデコレータ (`@property` / `@staticmethod` /
/// `@classmethod` / `@cached_property`) の付け外しは呼び出し方を変える (`u.name()` が
/// TypeError になる)。signature に反映して api.mod に出す。
#[test]
fn detect_api_changes_python_binding_decorator_change_is_modified() {
    let api = python_models_change(
        "class User:\n    def name(self) -> str:\n        return \"x\"\n\n    @staticmethod\n    def build(value):\n        return value\n",
        "class User:\n    @property\n    def name(self) -> str:\n        return \"x\"\n\n    @classmethod\n    def build(value):\n        return value\n",
    );
    let name = api
        .modified
        .iter()
        .find(|m| m.name == "User.name")
        .unwrap_or_else(|| {
            panic!(
                "@property の付与は api.mod に出すべき。modified={:?}",
                api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        name.old_signature.as_deref(),
        Some("def name(self) -> str:")
    );
    assert_eq!(
        name.new_signature.as_deref(),
        Some("@property def name(self) -> str:")
    );
    assert!(
        api.modified.iter().any(|m| m.name == "User.build"),
        "@staticmethod → @classmethod も api.mod に出すべき。modified={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );

    // 対照: 束縛を変えないデコレータの付与と、モジュール修飾だけの書き換えは api 差分にしない
    let api = python_models_change(
        "from functools import cached_property\n\n\nclass User:\n    def name(self) -> str:\n        return \"x\"\n\n    @cached_property\n    def build(self):\n        return 1\n",
        "import functools\n\n\nclass User:\n    @functools.lru_cache\n    def name(self) -> str:\n        return \"x\"\n\n    @functools.cached_property\n    def build(self):\n        return 1\n",
    );
    let changed: Vec<&str> = api
        .modified
        .iter()
        .chain(&api.modified_closed_in_diff)
        .map(|m| m.name.as_str())
        .chain(api.compatible_modified.iter().map(|m| m.name.as_str()))
        .collect();
    assert!(
        changed.is_empty(),
        "@lru_cache の付与や cached_property のモジュール修飾は呼び出し方を変えない。changed={changed:?}"
    );
}

/// signature への反映は束縛を変えるデコレータだけで、正規形 (`@cached_property`) に畳む。
#[test]
fn python_function_signature_includes_only_binding_decorators() {
    let src = "import functools\n\n\nclass User:\n    @functools.cached_property  # cache\n    def slug(self) -> str:\n        return \"x\"\n\n    @functools.lru_cache\n    def name(self) -> str:\n        return \"x\"\n\n\ndef helper(a):\n    return a\n";
    let syms = extract_exported_symbols_from_source("m.py", src.as_bytes()).expect("parse");
    let sig = |name: &str| {
        syms.iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, _, s)| s.as_str())
            .unwrap_or_else(|| panic!("{name} が抽出されない: {syms:?}"))
    };
    assert_eq!(sig("User.slug"), "@cached_property def slug(self) -> str:");
    assert_eq!(sig("User.name"), "def name(self) -> str:");
    assert_eq!(sig("helper"), "def helper(a):");
}

// ---------------------------------------------------------------------------
// TypedDict の `total=` 変更を型契約変更として分類する
// (Issue 2026-08-18-python-typeddict-contract-change-classification)
// ---------------------------------------------------------------------------

/// ファイル全体を 1 hunk として覆う `DiffFile` を作る。
/// closed-in-diff 判定に「参照行が変更 hunk に含まれるか」を渡すため、降格させたい側の
/// テストで意図せず hunk 外になるのを防ぐ。
fn whole_file_diff(path: &str, line_count: usize) -> crate::models::impact::DiffFile {
    crate::models::impact::DiffFile {
        old_path: path.to_string(),
        new_path: path.to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: line_count,
            new_start: 1,
            new_count: line_count,
        }],
        deleted_old_source: None,
    }
}

fn contract_of<'a>(
    api: &'a ApiChanges,
    name: &str,
) -> Option<&'a crate::models::review::ApiContractChange> {
    api.modified
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| c.contract_change.as_ref())
}

/// black / ruff が折り返したクラスヘッダでも `total=` の反転を分類できること。
///
/// `extract_api_signature` は class に対して先頭行だけを返していたため、
/// `class Payload(\n    TypedDict,\n    total=False,\n):` のヘッダが `class Payload(` になり、
/// `python_contract.rs` の前段フィルタ (`old_sig` / `new_sig` に `total` の字面があるか) を
/// 通過できず contract ラベルが失われていた。
///
/// レポートの暫定案「丸括弧が閉じていない signature は TypedDict 候補扱いにする」は採らない。
/// 3 値判定では解析に失敗すると `PotentialBreakingChange` へ倒れるため、無関係な複数行
/// Python class が広く blocking 化する。signature 抽出側を正しくするのが本筋。
///
/// 対照として 1 行ヘッダが従来どおり分類されること
/// (`typed_dict_total_false_removal_is_classified_as_producer_break`) も維持している。
#[test]
fn typed_dict_wrapped_header_total_change_is_still_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(
    TypedDict,
    total=False,
):
    a: int
    b: str
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict


class Payload(
    TypedDict,
):
    a: int
    b: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 9)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract = contract_of(&api, "Payload")
        .expect("折り返しヘッダでも total=False の除去は型契約変更として分類されるべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Producer
    );
}

#[test]
fn typed_dict_total_false_removal_is_classified_as_producer_break() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
    b: str
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract =
        contract_of(&api, "Payload").expect("total=False の除去は型契約変更として分類されるべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Producer,
        "省略可キーの必須化で壊れるのは値を作る側"
    );
}

#[test]
fn typed_dict_total_false_addition_is_classified_as_consumer_break() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
    b: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract =
        contract_of(&api, "Payload").expect("total=False の追加は型契約変更として分類されるべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictTotalFalseAdded
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Consumer,
        "必須キーが省略可になって壊れるのは値を読む側"
    );
}

/// fail-open 回帰テスト。値を作る側 (producer) が別ファイルにあり、同一 diff 内で完全に
/// 更新済みでも、`total=False` の除去は blocking な `api.mod` に残さなければならない。
/// 外部リポジトリ / 動的生成された dict は静的に追えないため。
///
/// **前提の明記**: 現状 Python の class は `is_modified_closed_in_diff` の call ref 判定で
/// `Payload(a=1)` が Call として記録されず (`refs` の kind は `ref`)、`call_refs` が空になって
/// そもそも降格経路に到達しない。したがってバケットの assert は「今日の降格を止めている証明」
/// ではなく、**Python の参照解決が強化されて class が降格対象になった日に効く前方ガード**である
/// (それを目指す Issue が実在する: 2026-08-18-api-mod-python-method-owner-aware-resolution)。
///
/// 今日この時点で非 vacuous なのは `contract_change` の assert で、
/// `classify_signature_change` の早期 return を外すと失敗することを確認済み。
/// 降格経路そのものが生きていることは
/// `non_typed_dict_change_with_updated_callers_is_still_demoted` (関数版) が担保する。
#[test]
fn typed_dict_total_change_is_never_demoted_even_if_callers_updated_in_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let models_before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
    b: str
";
    let producer_before = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1)
";
    git_commit_files(
        repo,
        &[
            ("models.py", models_before),
            ("producer.py", producer_before),
        ],
        "initial",
    );

    let models_after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    // producer も同一 diff 内で必須化に追随する
    let producer_after = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1, b=\"x\")
";
    fs::write(repo.join("models.py"), models_after).expect("write");
    fs::write(repo.join("producer.py"), producer_after).expect("write");

    let diff_files = vec![
        whole_file_diff("models.py", 6),
        whole_file_diff("producer.py", 5),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|c| c.name == "Payload"),
        "呼び出し側が同一 diff で更新済みでも blocking な api.mod に残すこと。got modified: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Payload"),
        "modified_closed_in_diff へ降格してはならない"
    );
    assert!(
        !api.compatible_modified.iter().any(|c| c.name == "Payload"),
        "compatible_modified へ降格してはならない"
    );
    assert_eq!(
        contract_of(&api, "Payload").map(|c| c.kind),
        Some(crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved)
    );
}

/// 「全部 blocking のまま」でも通るテストにしないための対照ケース。
/// 同じ形 (別ファイルの参照を同一 diff で更新) でも、TypedDict でない**関数**の
/// シグネチャ変更は従来どおり `modified_closed_in_diff` へ降格する。
/// これが赤くなったら降格経路自体が壊れており、上の非降格テストも意味を失う。
#[test]
fn non_typed_dict_change_with_updated_callers_is_still_demoted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let api_before = "\
def build(a: int) -> int:
    return a
";
    let caller_before = "\
from api import build


def run() -> int:
    return build(1)
";
    git_commit_files(
        repo,
        &[("api.py", api_before), ("caller.py", caller_before)],
        "initial",
    );

    let api_after = "\
def build(a: int, b: str) -> int:
    return a + len(b)
";
    let caller_after = "\
from api import build


def run() -> int:
    return build(1, \"x\")
";
    fs::write(repo.join("api.py"), api_after).expect("write");
    fs::write(repo.join("caller.py"), caller_after).expect("write");

    let diff_files = vec![
        whole_file_diff("api.py", 2),
        whole_file_diff("caller.py", 5),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "build"),
        "対照ケースは従来どおり降格すること (降格経路が生きている証明)。\
         got modified: {:?} / closed: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn omitted_total_to_explicit_true_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    // 実効 total は変わらない (省略 = True) ため、素の api.mod に留める。
    let after = "\
from typing import TypedDict


class Payload(TypedDict, total=True):
    a: int
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 5)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Payload").is_none(),
        "省略 ↔ total=True は意味的に同値なので分類しない"
    );
}

#[test]
fn total_change_on_non_typed_dict_class_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class Base:
    pass


class Payload(Base, total=False):
    a: int
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
class Base:
    pass


class Payload(Base):
    a: int
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Payload").is_none(),
        "TypedDict と証明できない基底クラスでは分類しない"
    );
    assert!(
        api.modified.iter().any(|c| c.name == "Payload")
            || api
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Payload"),
        "分類できなくても従来どおり api.mod としては検出されること"
    );
}

#[test]
fn total_change_with_not_required_field_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict, NotRequired


class Payload(TypedDict, total=False):
    a: int
    b: NotRequired[str]
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict, NotRequired


class Payload(TypedDict):
    a: int
    b: NotRequired[str]
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract = contract_of(&api, "Payload").expect(
        "修飾子が同居していても、修飾子なしフィールド (a: int) の requiredness は total で動くので分類できる",
    );
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Producer
    );
    // 対照: 修飾子付きフィールドしか無ければ total を反転しても実効値は動かないので
    // 種別を付けない (blocking は維持される。`unclassifiable_total_change_is_still_not_demoted`)。
    assert!(
        !api.modified.iter().any(|c| c.name == "Payload.b"),
        "total 変更はクラス単位で報告し、修飾子付きフィールドを二重に出さない: {:?}",
        api.modified
    );
}

#[test]
fn total_change_with_simultaneous_field_change_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    // total とフィールド追加が同時に起きると「壊れる側」が一意に決まらない。
    let after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Payload").is_none(),
        "total 以外の変更が混ざる場合は分類しない"
    );
}

/// **severity ガードの回帰テスト** (レビュー 3 巡目の指摘)。
///
/// **種別を確定できない** `total=` 変更でも、blocking な `api.mod` に残さなければならない。
/// 分類できないことは「破壊的でない」証明ではない。
///
/// ここでは唯一のフィールドが別モジュール由来の `MyModel` で、それが `NotRequired[...]` の
/// 型エイリアスである可能性を静的に排除できないため実効 requiredness を決められない。
/// それでも実際には反転している可能性があるので blocking は維持する。
///
/// 前提は `typed_dict_total_change_is_never_demoted_even_if_callers_updated_in_diff` と同じで、
/// 現状 Python の class は降格経路に到達しないため、バケットの assert は前方ガードとして効く。
#[test]
fn unclassifiable_total_change_is_still_not_demoted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let models_before = "\
from typing import TypedDict
from mylib import MyModel


class Payload(TypedDict, total=False):
    a: MyModel
";
    let producer_before = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1)
";
    git_commit_files(
        repo,
        &[
            ("models.py", models_before),
            ("producer.py", producer_before),
        ],
        "initial",
    );

    let models_after = "\
from typing import TypedDict
from mylib import MyModel


class Payload(TypedDict):
    a: MyModel
";
    let producer_after = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1)
";
    fs::write(repo.join("models.py"), models_after).expect("write");
    fs::write(repo.join("producer.py"), producer_after).expect("write");

    let diff_files = vec![
        whole_file_diff("models.py", 7),
        whole_file_diff("producer.py", 5),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|c| c.name == "Payload"),
        "種別を確定できなくても blocking な api.mod に残すこと。got modified: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Payload"),
        "分類できないことを理由に降格してはならない"
    );
    assert!(
        !api.compatible_modified.iter().any(|c| c.name == "Payload"),
        "compatible_modified へ降格してはならない"
    );
    assert!(
        contract_of(&api, "Payload").is_none(),
        "証明できていないのでラベルは付けない"
    );
}

/// **severity ガードの回帰テスト 2** (レビュー 4 巡目の指摘)。
///
/// 同一ファイル内でしか使われず cross-file 参照が 0 件のシンボルは、通常
/// `is_internally_connected` の早期除外で api.mod にすら出ない。しかし TypedDict の
/// `total=` 変更はキーの必須化という破壊的変更なので、この除外に落としてはならない。
#[test]
fn typed_dict_total_change_used_only_in_same_file_is_still_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int


def build() -> Payload:
    return Payload()
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int


def build() -> Payload:
    return Payload(a=1)
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 9)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|c| c.name == "Payload"),
        "同一ファイル内利用だけでも TypedDict の total 変更は api.mod に出すこと。got: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert_eq!(
        contract_of(&api, "Payload").map(|c| c.kind),
        Some(crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved)
    );
}

/// 対照ケース: TypedDict でない Python class は従来どおり早期除外される。
/// これが赤くなったら迂回が広すぎる (無関係なシンボルまで api.mod に出している)。
#[test]
fn non_typed_dict_class_used_only_in_same_file_is_still_excluded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class Base:
    pass


class Payload(Base, total=False):
    a: int


def build() -> Payload:
    return Payload()
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
class Base:
    pass


class Payload(Base):
    a: int


def build() -> Payload:
    return Payload()
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 10)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api.modified.iter().any(|c| c.name == "Payload"),
        "TypedDict と証明できないクラスは従来どおり早期除外されること。got: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}

#[test]
fn detect_api_changes_python_implicit_string_concat_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = concat!(
        "def describe(value: str) -> str:\n",
        "    return value\n\n",
        "def handler(label: str = describe(\"alpha beta gamma\")) -> None:\n",
        "    pass\n"
    );
    let caller = "from api import handler\nhandler()\n";
    git_commit_files(
        repo,
        &[("api.py", before), ("caller.py", caller)],
        "initial",
    );

    let after = concat!(
        "def describe(value: str) -> str:\n",
        "    return value\n\n",
        "def handler(\n",
        "    label: str = describe(\n",
        "        \"alpha \"\n",
        "        \"beta \"\n",
        "        \"gamma\"\n",
        "    ),\n",
        ") -> None:\n",
        "    pass\n"
    );
    fs::write(repo.join("api.py"), after).expect("write");

    let api = detect_api_changes(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        &[whole_file_diff("api.py", 11)],
    );
    assert!(
        !api.modified.iter().any(|change| change.name == "handler"),
        "同値な暗黙文字列連結は blocking に残さない: {api:?}"
    );
    assert!(api.compatible_modified.iter().any(|change| {
        change.name == "handler" && change.reason == "equivalent_implicit_string_concat"
    }));
}

#[test]
fn detect_api_changes_python_implicit_string_concat_counterexamples_stay_modified() {
    let cases = [
        (
            "def handler(value: str = \"alpha beta\") -> None:\n    pass\n",
            "def handler(value: str = (\"alpha\" \"beta\")) -> None:\n    pass\n",
        ),
        (
            "def handler(value: str = f\"alpha {name}\") -> None:\n    pass\n",
            "def handler(value: str = f\"alpha {other}\") -> None:\n    pass\n",
        ),
        (
            "def handler(value: str = \"alpha beta\") -> None:\n    pass\n",
            "def handler(value: str = \"different\") -> None:\n    pass\n",
        ),
    ];

    for (before, after) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("api.py", before),
                ("caller.py", "from api import handler\nhandler()\n"),
            ],
            "initial",
        );
        fs::write(repo.join("api.py"), after).expect("write");

        let api = detect_api_changes(
            repo.to_str().expect("utf-8 path"),
            "HEAD",
            &[whole_file_diff("api.py", 2)],
        );
        assert!(
            api.modified.iter().any(|change| change.name == "handler"),
            "値変更・f-string・別 default は blocking を維持する: {api:?}"
        );
        assert!(
            !api.compatible_modified
                .iter()
                .any(|change| change.name == "handler"),
            "反例を compatible_modified へ降格しない: {api:?}"
        );
    }
}

/// `y: NotRequired[str]` → `y: str` (省略可キーの必須化) を検出すること。
///
/// クラスヘッダ行が変わらないため既存のシグネチャ差分ループには乗らない。
/// 独立した contract 経路が拾う (Issue
/// 2026-08-19-python-typeddict-field-requiredness-detection)。
#[test]
fn typed_dict_not_required_removal_is_classified_as_producer_break() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import NotRequired, TypedDict


class Payload(TypedDict):
    x: int
    y: NotRequired[str]
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import NotRequired, TypedDict


class Payload(TypedDict):
    x: int
    y: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    eprintln!("modified: {:?}", api.modified);

    let contract = contract_of(&api, "Payload.y")
        .expect("NotRequired の除去はフィールド単位の型契約変更として分類されるべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictFieldBecameRequired
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Producer
    );
}

/// `y: str` → `y: NotRequired[str]` (必須キーの省略可化) を検出すること。
///
/// 対照として、同じ diff に置いた「表記も requiredness も変わらないフィールド」と
/// 「型だけが変わったフィールド」が出ないことを同時に固定する。後者を出してしまうと
/// 「requiredness の変更」というラベルが型変更にも付く。
#[test]
fn typed_dict_not_required_addition_is_classified_as_consumer_break() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import NotRequired, TypedDict


class Payload(TypedDict):
    keep: NotRequired[int]
    widened: str
    retyped: NotRequired[int]
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import NotRequired, TypedDict


class Payload(TypedDict):
    keep: NotRequired[int]
    widened: NotRequired[str]
    retyped: NotRequired[float]
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 7)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract = contract_of(&api, "Payload.widened")
        .expect("NotRequired の付与はフィールド単位の型契約変更として分類されるべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictFieldBecameNotRequired
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Consumer,
        "必須キーの省略可化で壊れるのは値を読む側"
    );

    assert!(
        contract_of(&api, "Payload.keep").is_none(),
        "requiredness が変わらないフィールドは出さない"
    );
    assert!(
        contract_of(&api, "Payload.retyped").is_none(),
        "型が変わったフィールドは『requiredness だけが動いた』と言えないので出さない: {:?}",
        api.modified
    );
}

/// requiredness 変更は、リポジトリ内の参照が同一 diff で更新済みでも blocking に残ること。
///
/// 外部リポジトリや動的に組み立てた dict は静的に追えないため、`total=` 変更と同じ理由で
/// 降格させてはいけない。対照として、同じ diff 内の**通常の Python 関数**が従来どおり
/// 扱われること (契約ラベルが付かないこと) を同時に固定する。
#[test]
fn typed_dict_field_requiredness_stays_blocking_when_all_callers_updated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let models_before = "\
from typing import NotRequired, TypedDict


class Payload(TypedDict):
    x: int
    y: NotRequired[str]


def build(x: int) -> Payload:
    return {\"x\": x}
";
    let caller_before = "\
from models import Payload, build


def run() -> Payload:
    return build(1)
";
    git_commit_files(
        repo,
        &[("models.py", models_before), ("caller.py", caller_before)],
        "initial",
    );

    // y を必須化し、同一 diff で唯一の呼び出し側も追随させる。
    let models_after = "\
from typing import NotRequired, TypedDict


class Payload(TypedDict):
    x: int
    y: str


def build(x: int, y: str) -> Payload:
    return {\"x\": x, \"y\": y}
";
    let caller_after = "\
from models import Payload, build


def run() -> Payload:
    return build(1, \"z\")
";
    fs::write(repo.join("models.py"), models_after).expect("write");
    fs::write(repo.join("caller.py"), caller_after).expect("write");

    let diff_files = vec![
        whole_file_diff("models.py", 10),
        whole_file_diff("caller.py", 5),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract = contract_of(&api, "Payload.y")
        .expect("呼び出し側を全て更新しても requiredness 変更は blocking な api.mod に残るべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictFieldBecameRequired
    );
    assert!(
        !api.compatible_modified
            .iter()
            .any(|c| c.name == "Payload.y"),
        "契約変更を互換扱いへ降格させてはいけない"
    );
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Payload.y"),
        "契約変更を『同一 diff で解決済み』へ降格させてはいけない"
    );

    // 対照: 同じ diff の通常関数は契約ラベルを持たない (この経路が Python の class 以外へ
    // 波及していないこと)。
    assert!(
        contract_of(&api, "build").is_none(),
        "通常の関数シグネチャ変更に契約ラベルは付かない"
    );
}

/// 公開判定は既存の exported symbol 判定と共有する。
///
/// `__all__` に載っていないクラスのフィールド requiredness が変わっても報告しない。
/// contract 判定に独自の公開判定を持たせると、`symbols` / `dead-code` と食い違う。
/// 対照として、同じ diff の `__all__` 掲載クラスは従来どおり報告されることを固定する。
#[test]
fn typed_dict_field_change_respects_dunder_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import NotRequired, TypedDict

__all__ = [\"Public\"]


class Public(TypedDict):
    x: int
    y: NotRequired[str]


class Internal(TypedDict):
    x: int
    y: NotRequired[str]
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import NotRequired, TypedDict

__all__ = [\"Public\"]


class Public(TypedDict):
    x: int
    y: str


class Internal(TypedDict):
    x: int
    y: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 14)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Public.y").is_some(),
        "__all__ に載っているクラスは報告する: {:?}",
        api.modified
    );
    assert!(
        contract_of(&api, "Internal.y").is_none(),
        "__all__ に載っていないクラスは公開 API 面ではないので報告しない: {:?}",
        api.modified
    );
}

/// `__all__` はモジュールトップレベルの名前だけを支配し、クラスメンバーには効かない。
///
/// 既存モジュールに `__all__` を追加しただけで、そのモジュールの全クラスの全メソッドが
/// 「公開 API の削除 (api.rm)」として報告されていた (メソッド名は `from m import *` の
/// 対象になり得ないので `__all__` には載せられない = 常に membership 判定に落ちるため)。
/// 対照として、トップレベルのクラスには従来どおり `__all__` が効くことを同じテストで固定する
/// (片方だけだと「`__all__` を一切見なくなった」退行を検出できない)。
#[test]
fn dunder_all_does_not_apply_to_class_members() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class Widget:
    def compute_total(self, a: int, b: int) -> int:
        return a + b


class Helper:
    def assist(self) -> int:
        return 0
";
    git_commit_files(repo, &[("m.py", before)], "initial");

    // `__all__` を追加し、あわせてメソッドへ末尾の任意引数を足す (後方互換な変更)。
    let after = "\
__all__ = [\"Widget\"]


class Widget:
    def compute_total(self, a: int, b: int, *, scale: int = 1) -> int:
        return (a + b) * scale


class Helper:
    def assist(self) -> int:
        return 0
";
    fs::write(repo.join("m.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("m.py", 11)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.contains(&"Widget.compute_total"),
        "`__all__` の追加でクラスメソッドが削除扱いになってはならない: {removed:?}"
    );
    assert!(
        api.compatible_modified
            .iter()
            .any(|c| c.name == "Widget.compute_total"),
        "末尾任意引数の追加は互換変更として報告する: {:?}",
        api.compatible_modified
    );

    // 対照: トップレベルの名前には `__all__` が効き続ける。`Helper` は `__all__` に
    // 載っていないので `from m import *` で束縛されなくなる = 公開 API 面からの削除であり、
    // 報告されるのが正しい (これが「`__all__` を一切見なくなった」退行の検出器になる)。
    // 一方 `Widget` は `__all__` に載っているので公開面に残る。
    assert!(
        removed.contains(&"Helper"),
        "`__all__` から外れたトップレベルクラスは公開 API 面の削除として報告する: {removed:?}"
    );
    assert!(
        !removed.contains(&"Widget"),
        "`__all__` に載っているクラスは公開面に残る: {removed:?}"
    );
}

/// `__all__` を完全に評価できない形では `_` 規約へフォールバックする (fail-closed)。
///
/// 旧実装は最初の `__all__ = [...]` だけを完全な集合として採用し、後続の `+=` /
/// `.extend()` を黙って無視していたため、そこで追加された名前が「非公開」に落ち、
/// 削除もシグネチャ変更も報告されない沈黙する検出漏れになっていた。
#[test]
fn dunder_all_mutation_falls_back_to_underscore_convention() {
    for (label, header) in [
        (
            "augmented",
            "__all__ = [\"Alpha\"]\n__all__ += [\"Beta\"]\n",
        ),
        (
            "extend",
            "__all__ = [\"Alpha\"]\n__all__.extend([\"Beta\"])\n",
        ),
        ("spread", "__all__ = [\"Alpha\", *_EXTRA]\n"),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);

        let before = format!(
            "_EXTRA = [\"Beta\"]\n{header}\n\nclass Alpha:\n    pass\n\n\nclass Beta:\n    pass\n\n\nclass _Private:\n    pass\n"
        );
        git_commit_files(repo, &[("m.py", before.as_str())], "initial");

        // `Beta` と `_Private` を削除する。
        let after = format!("_EXTRA = [\"Beta\"]\n{header}\n\nclass Alpha:\n    pass\n");
        fs::write(repo.join("m.py"), after.as_str()).expect("write");

        let diff_files = vec![whole_file_diff("m.py", 14)];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

        let removed: Vec<&str> = api
            .removed
            .iter()
            .chain(api.removed_dead.iter())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            removed.contains(&"Beta"),
            "[{label}] 評価できない `__all__` では `_` 規約へ倒し、Beta の削除を報告する: {removed:?}"
        );
        // 対照: フォールバック先は `_` 規約なので、`_` 始まりは引き続き非公開。
        assert!(
            !removed.contains(&"_Private"),
            "[{label}] `_` 始まりは非公開のまま: {removed:?}"
        );
    }
}

/// 対照: 単純な `__all__` は従来どおり集合として尊重する。
///
/// 上のフォールバックが効きすぎて「`__all__` を常に無視する」退行になっていないかを固定する。
#[test]
fn simple_dunder_all_still_restricts_toplevel_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
__all__ = [\"Alpha\"]


class Alpha:
    pass


class Beta:
    pass
";
    git_commit_files(repo, &[("m.py", before)], "initial");

    let after = "\
__all__ = [\"Alpha\"]


class Alpha:
    pass
";
    fs::write(repo.join("m.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("m.py", 9)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.contains(&"Beta"),
        "`__all__` に載っていないトップレベルクラスの削除は公開 API 変更ではない: {removed:?}"
    );
}

/// 公開 Literal 型エイリアスの値集合変更は、通常の symbol 抽出に混ぜず API 契約変更として出す。
///
/// 同一 diff で呼び出し側が追随していても、外部利用者の入力までは確認できないため blocking を
/// 維持する。対照として `__all__` 非掲載の型エイリアスは報告しない。
#[test]
fn literal_alias_narrowing_stays_blocking_when_callers_are_updated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let models_before = r#"from typing import Literal

__all__ = ["Mode"]

Mode = Literal["read", "write"]
Hidden = Literal["read", "write"]
"#;
    let caller_before = r#"from models import Mode

def run(mode: Mode = "write") -> Mode:
    return mode
"#;
    git_commit_files(
        repo,
        &[("models.py", models_before), ("caller.py", caller_before)],
        "initial",
    );

    let models_after = models_before
        .replace(
            "Mode = Literal[\"read\", \"write\"]",
            "Mode = Literal[\"read\"]",
        )
        .replace(
            "Hidden = Literal[\"read\", \"write\"]",
            "Hidden = Literal[\"read\"]",
        );
    let caller_after = caller_before.replace("= \"write\"", "= \"read\"");
    fs::write(repo.join("models.py"), models_after).expect("write models");
    fs::write(repo.join("caller.py"), caller_after).expect("write caller");

    let diff_files = vec![
        whole_file_diff("models.py", 7),
        whole_file_diff("caller.py", 4),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract = contract_of(&api, "Mode")
        .expect("公開 Literal の縮小は、呼び出し側が追随しても api.mod に残るべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::LiteralValuesNarrowed
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Producer
    );
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|change| change.name == "Mode"),
        "契約変更を『同一 diff で解決済み』へ降格させてはいけない"
    );
    assert!(
        !api.compatible_modified
            .iter()
            .any(|change| change.name == "Mode"),
        "契約変更を互換扱いへ降格させてはいけない"
    );
    assert!(
        !api.modified.iter().any(|change| change.name == "Hidden"),
        "__all__ 非掲載の型エイリアスは、ラベルの有無によらず api.mod に出してはならない"
    );
}

/// テストファイルは公開 API 面から除外し、同じ変更でも通常ソースなら検出する。
#[test]
fn literal_alias_change_ignores_test_path_but_reports_production_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    fs::create_dir_all(repo.join("tests")).expect("create tests directory");

    let test_before = "from typing import Literal\n\nTestMode = Literal[\"read\", \"write\"]\n";
    let production_before =
        "from typing import Literal\n\nProductionMode = Literal[\"read\", \"write\"]\n";
    git_commit_files(
        repo,
        &[
            ("tests/test_modes.py", test_before),
            ("models.py", production_before),
        ],
        "initial",
    );

    fs::write(
        repo.join("tests/test_modes.py"),
        test_before.replace(", \"write\"", ""),
    )
    .expect("write test module");
    fs::write(
        repo.join("models.py"),
        production_before.replace(", \"write\"", ""),
    )
    .expect("write production module");

    let diff_files = vec![
        whole_file_diff("tests/test_modes.py", 3),
        whole_file_diff("models.py", 3),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "ProductionMode").is_some(),
        "通常ソースの Literal 縮小は対照として検出されるべき"
    );
    assert!(
        !api.modified.iter().any(|change| change.name == "TestMode"),
        "テストファイルの Literal 縮小は api.mod に出してはならない"
    );
}

/// 定義と呼び出し側を旧版でコミットし、作業ツリーを新版に書き換えて API 差分を検出する。
fn python_return_change(
    definition: (&str, &str),
    caller: (&str, &str),
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("app.py", definition.0), ("test_app.py", caller.0)],
        "initial",
    );
    fs::write(repo.join("app.py"), definition.1).expect("write definition");
    fs::write(repo.join("test_app.py"), caller.1).expect("write caller");
    detect_api_changes_from_worktree(repo)
}

/// `name` が `modified_closed_in_diff` に入り、`modified` に残らないか (どちらか一方に入ることも確かめる)。
fn closed_in_diff(changes: &crate::models::review::ApiChanges, name: &str) -> bool {
    let closed = changes
        .modified_closed_in_diff
        .iter()
        .any(|c| c.name == name);
    let blocking = changes.modified.iter().any(|c| c.name == name);
    assert_ne!(closed, blocking, "{name}: {changes:?}");
    closed
}

const PY_ENTRY_BEFORE: &str =
    "def build():\n    return \"done\"\n\n\ndef entry():\n    return build()\n";
const PY_ENTRY_AFTER: &str =
    "def build():\n    return \"done\"\n\n\ndef entry() -> int:\n    return 0 if build() else 1\n";
const PY_ENTRY_CALLER_BEFORE: &str = "from app import entry


def test_entry_returns_text():
    result = entry()
    assert result == \"done\"
";

/// 戻り値の注釈だけを変えた関数の呼び出しを、結果の比較の書き換えで閉じる (Issue #251)。
///
/// git diff は呼び出しの文 (`result = entry()`) を旧版の同じ文字列の行と対応付けて context
/// 行にする。結果の読み取りがすべて新しい型 (`int`) のリテラルとの比較で、その文が旧版に
/// 無いときだけ閉じる。改名・コピー・切り出しで旧版と同じ比較が残る形、一部の比較だけを
/// 直した形、比較の相手が変数・別の型の形、結果を別の名前や関数へ渡す形は blocking に残す。
#[test]
fn python_return_annotation_change_closes_when_result_comparisons_follow_the_new_type() {
    for (label, before, after, should_close) in [
        (
            // Issue の再現: 旧版の関数は entry を呼ばなくなり、新しい関数が旧版と同じ文字列の行で呼ぶ。
            "new function reuses the old call line",
            PY_ENTRY_CALLER_BEFORE,
            "from app import build, entry


def test_build_returns_text():
    assert build() == \"done\"


def test_entry_returns_exit_code():
    result = entry()
    assert result == 0
",
            true,
        ),
        (
            "same function rewrites the assertion",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    result = entry()
    assert result in (0, 1), f\"unexpected {result}\"
",
            true,
        ),
        (
            // 受け入れ条件 2: 関数は旧版のまま本体の別の行だけを直し、呼び出しも使い方も旧契約。
            "same function changes only another line",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    print(\"checking\")
    result = entry()
    assert result == \"done\"
",
            false,
        ),
        (
            "rename only",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_string():
    result = entry()
    assert result == \"done\"
",
            false,
        ),
        (
            "rename and rewrite an unrelated line",
            "from app import entry


def test_entry_returns_text():
    print(\"a\")
    result = entry()
    assert result == \"done\"
",
            "from app import entry


def test_entry_returns_string():
    print(\"b\")
    result = entry()
    assert result == \"done\"
",
            false,
        ),
        (
            "copy the old body under a new name",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    assert entry() == 0


def test_entry_returns_copy():
    result = entry()
    assert result == \"done\"
",
            false,
        ),
        (
            "extract the old check into a helper",
            "from app import entry


def test_entry():
    print(\"start\")
    result = entry()
    assert result == \"done\"
",
            "from app import entry


def test_entry():
    print(\"start\")
    check()


def check():
    result = entry()
    assert result == \"done\"
",
            false,
        ),
        (
            "add a new comparison but keep the old one",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    result = entry()
    assert result == 0
    assert result != \"done\"
",
            false,
        ),
        (
            "compare with a variable",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    result = entry()
    expected = 0
    assert result == expected
",
            false,
        ),
        (
            "compare with a literal of another type",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    result = entry()
    assert result == \"0\"
",
            false,
        ),
        (
            "pass the result to a function",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    result = entry()
    check(result)
    assert result == 0
",
            false,
        ),
        (
            "hand the result to another name",
            PY_ENTRY_CALLER_BEFORE,
            "from app import entry


def test_entry_returns_text():
    result = entry()
    value = result
    assert value == 0
",
            false,
        ),
        (
            // 新しい型に合う比較でも、旧版と同じ文なら持ち越しで、この差分で直した証拠にならない。
            "rename keeps an old comparison that already fits",
            "from app import entry


def test_entry_returns_text():
    result = entry()
    assert result == 0
",
            "from app import entry


def test_entry_returns_zero():
    result = entry()
    assert result == 0
",
            false,
        ),
        (
            // 旧版の比較のコメントだけを変えても、比べる値は書き換えていない。
            "only the comment of an old comparison changes",
            "from app import entry


def test_entry_returns_text():
    result = entry()
    assert result == 0
",
            "from app import entry


def test_entry_returns_text():
    result = entry()
    assert result == 0  # exit code
",
            false,
        ),
        (
            "module-level call is not followed",
            "from app import entry\n\nresult = entry()\nassert result == \"done\"\n",
            "from app import entry\n\nresult = entry()\nassert result == 0\n",
            false,
        ),
    ] {
        let changes = python_return_change((PY_ENTRY_BEFORE, PY_ENTRY_AFTER), (before, after));
        assert_eq!(
            closed_in_diff(&changes, "entry"),
            should_close,
            "{label}: {changes:?}"
        );
    }
}

/// 結果の比較を証拠にするのは、名前・`async`・型引数・引数・すべてのデコレータが同じで、
/// 戻り値の注釈だけが扱える型に変わったときに限る。引数の変更は呼び出し式の追随が要り、
/// `async` 化・デコレータの付け外し・generator・束縛し直した型名では、結果が注釈の型の値と
/// 言えない。
#[test]
fn python_return_comparison_evidence_requires_a_return_annotation_only_change() {
    let rewritten_caller = |call: &str, wrapper: &str| {
        (
            format!(
                "from app import entry\n\n\n{wrapper}def test_entry_returns_text():\n    result = {call}\n    assert result == \"done\"\n"
            ),
            format!(
                "from app import entry\n\n\n{wrapper}def test_entry_returns_text():\n    result = {call}\n    assert result == 0\n"
            ),
        )
    };
    for (label, definition_before, definition_after, call, wrapper, should_close) in [
        (
            "return annotation only",
            "def entry():\n    return \"done\"\n",
            "def entry() -> int:\n    return 0\n",
            "entry()",
            "",
            true,
        ),
        (
            "awaited async function",
            "async def entry():\n    return \"done\"\n",
            "async def entry() -> int:\n    return 0\n",
            "await entry()",
            "async ",
            true,
        ),
        (
            "parameter annotation also changes",
            "def entry(value):\n    return \"done\"\n",
            "def entry(value: int) -> int:\n    return value\n",
            "entry(1)",
            "",
            false,
        ),
        (
            "function becomes async",
            "def entry():\n    return \"done\"\n",
            "async def entry() -> int:\n    return 0\n",
            "entry()",
            "",
            false,
        ),
        (
            "decorator is removed",
            "import functools\n\n\n@functools.cache\ndef entry():\n    return \"done\"\n",
            "import functools\n\n\ndef entry() -> int:\n    return 0\n",
            "entry()",
            "",
            false,
        ),
        (
            "function becomes a generator",
            "def entry():\n    return \"done\"\n",
            "def entry() -> int:\n    yield 0\n",
            "entry()",
            "",
            false,
        ),
        (
            "annotation name is rebound",
            "def entry():\n    return \"done\"\n",
            "int = float\n\n\ndef entry() -> int:\n    return 0\n",
            "entry()",
            "",
            false,
        ),
        (
            "annotation is not a plain builtin",
            "def entry():\n    return \"done\"\n",
            "def entry() -> list[int]:\n    return [0]\n",
            "entry()",
            "",
            false,
        ),
    ] {
        let (caller_before, caller_after) = rewritten_caller(call, wrapper);
        let changes = python_return_change(
            (definition_before, definition_after),
            (&caller_before, &caller_after),
        );
        assert_eq!(
            closed_in_diff(&changes, "entry"),
            should_close,
            "{label}: {changes:?}"
        );
    }
}

/// メソッドの戻り値の注釈だけの変更も、受け手を確かめた呼び出しの結果の比較で閉じる。
#[test]
fn python_method_return_annotation_change_follows_result_comparisons() {
    let definition = (
        "class Service:\n    def price(self):\n        return \"1\"\n",
        "class Service:\n    def price(self) -> int:\n        return 1\n",
    );
    let caller_before = "from app import Service


def test_price():
    result = Service().price()
    assert result == \"1\"
";
    for (label, caller_after, should_close) in [
        (
            "comparison rewritten",
            "from app import Service


def test_price():
    result = Service().price()
    assert result == 1
",
            true,
        ),
        (
            "old comparison kept",
            "from app import Service


def test_price():
    print(\"checking\")
    result = Service().price()
    assert result == \"1\"
",
            false,
        ),
    ] {
        let changes = python_return_change(definition, (caller_before, caller_after));
        assert_eq!(
            closed_in_diff(&changes, "Service.price"),
            should_close,
            "{label}: {changes:?}"
        );
    }
}

/// 呼び出し側のファイルを改名した差分では、改名前のパスの旧版と比べる。
#[test]
fn python_return_comparison_evidence_reads_the_renamed_caller_from_its_old_path() {
    for (label, caller_after, should_close) in [
        (
            "comparison rewritten",
            "from app import entry


def test_entry_returns_text():
    print(\"renamed\")
    result = entry()
    assert result == 0
",
            true,
        ),
        (
            "old comparison kept",
            "from app import entry


def test_entry_returns_text():
    print(\"renamed\")
    result = entry()
    assert result == \"done\"
",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("app.py", PY_ENTRY_BEFORE),
                ("test_app.py", PY_ENTRY_CALLER_BEFORE),
            ],
            "initial",
        );
        git_in(repo, &["mv", "test_app.py", "test_entry.py"]);
        fs::write(repo.join("app.py"), PY_ENTRY_AFTER).expect("write definition");
        fs::write(repo.join("test_entry.py"), caller_after).expect("write caller");
        let changes = detect_api_changes_from_worktree(repo);
        assert_eq!(
            closed_in_diff(&changes, "entry"),
            should_close,
            "{label}: {changes:?}"
        );
    }
}
