use crate::commands::tests::common::{
    detect_api_changes_from_worktree, git_commit_files, init_git_repo_for_test,
};
use crate::models::python_import_rebinding::RebindingEquivalence;

const FUNCTION: &str = "def decorate(value: str) -> str:\n    return '<' + value + '>'\n";
const CLIENT: &str = "from api import decorate\nprint(decorate('x'))\n";

#[test]
fn python_import_rebinding_expression_nodes_exist_in_python_grammar() {
    let grammar = crate::language::LangId::Python.ts_language();
    for kind in [
        "block",
        "return_statement",
        "binary_operator",
        "unary_operator",
        "boolean_operator",
        "not_operator",
        "comparison_operator",
        "conditional_expression",
        "parenthesized_expression",
        "integer",
        "float",
        "true",
        "false",
        "none",
        "comment",
        "module",
        "assignment",
        "type",
        "function_definition",
        "typed_parameter",
        "default_parameter",
        "typed_default_parameter",
        "import_from_statement",
        "import_statement",
        "dotted_name",
        "aliased_import",
        "global_statement",
        "nonlocal_statement",
        "wildcard_import",
    ] {
        tree_sitter::Query::new(&grammar, &format!("({kind}) @node")).unwrap();
    }
}

fn rebind(api_after: &str, shared: &str, client: &str) -> crate::models::review::ApiChanges {
    rebind_with_extra(api_after, shared, client, &[])
}

fn rebind_with_extra(
    api_after: &str,
    shared: &str,
    client: &str,
    extra: &[(&str, &str)],
) -> crate::models::review::ApiChanges {
    let temporary = tempfile::tempdir().unwrap();
    let repo = temporary.path();
    init_git_repo_for_test(repo);
    let before = format!("{FUNCTION}\ndef own(value: str) -> str:\n    return decorate(value)\n");
    let mut files: Vec<(&str, &str)> = vec![
        ("api.py", &before),
        ("shared.py", shared),
        ("client.py", client),
    ];
    files.extend_from_slice(extra);
    git_commit_files(repo, &files, "base");
    std::fs::write(repo.join("api.py"), api_after).unwrap();
    detect_api_changes_from_worktree(repo)
}

#[test]
fn python_import_rebinding_module_initialization_annotations_stay_blocking() {
    for prefix in [
        "boom: 1 / 0 = 0\n",
        "def touch():\n    print('imported')\n    return int\nmarker: touch() = 1\n",
        "def unrelated(value: 1 / 0):\n    return value\n",
        "def unrelated(value=1 / 0):\n    return value\n",
        "def unrelated() -> 1 / 0:\n    return 0\n",
    ] {
        let api = rebind(
            "from shared import decorate as decorate\n",
            &format!("{prefix}{FUNCTION}"),
            CLIENT,
        );
        assert!(
            api.removed.iter().any(|entry| entry.name == "decorate"),
            "{prefix}: {api:?}"
        );
        assert!(
            api.import_rebindings
                .iter()
                .all(|entry| entry.name != "decorate"),
            "{prefix}: {api:?}"
        );
    }
}

#[test]
fn python_import_rebinding_same_body_module_global_bindings_stay_unverified() {
    let temporary = tempfile::tempdir().unwrap();
    let repo = temporary.path();
    init_git_repo_for_test(repo);
    let body = "def decorate(value: str) -> str:\n    return _prefix + value + '>'\n";
    let old = format!("_prefix = '<'\n{body}");
    let shared = format!("_prefix = '['\n{body}");
    git_commit_files(
        repo,
        &[
            ("api.py", &old),
            ("shared.py", &shared),
            ("client.py", CLIENT),
        ],
        "base",
    );
    std::fs::write(
        repo.join("api.py"),
        "from shared import decorate as decorate\n",
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.modified.iter().any(|entry| entry.name == "decorate"),
        "{api:?}"
    );
    assert!(
        api.compatible_modified
            .iter()
            .all(|entry| entry.name != "decorate"),
        "{api:?}"
    );
    assert_eq!(
        api.import_rebindings
            .iter()
            .find(|entry| entry.name == "decorate")
            .unwrap()
            .equivalence,
        RebindingEquivalence::Unverified
    );
}

#[test]
fn python_import_rebinding_reachable_ignored_source_stays_unverified() {
    let hidden = "def probe():\n    from api import decorate\n    return decorate.__module__\n";
    let client =
        "from api import decorate\nfrom hidden import probe\nprint(probe(), decorate('x'))\n";
    let api = rebind_with_extra(
        "from shared import decorate as decorate\n",
        FUNCTION,
        client,
        &[(".gitignore", "hidden.py\n")],
    );
    // 既知の import 先が欠けていれば、この時点でも同値性を証明しない。
    assert!(
        api.compatible_modified
            .iter()
            .all(|entry| entry.name != "decorate"),
        "{api:?}"
    );

    let temporary = tempfile::tempdir().unwrap();
    let repo = temporary.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("api.py", FUNCTION),
            ("shared.py", FUNCTION),
            ("client.py", client),
            (".gitignore", "hidden.py\n"),
        ],
        "base",
    );
    std::fs::write(repo.join("hidden.py"), hidden).unwrap();
    std::fs::write(
        repo.join("api.py"),
        "from shared import decorate as decorate\n",
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.modified.iter().any(|entry| entry.name == "decorate"),
        "{api:?}"
    );
    assert!(
        api.compatible_modified
            .iter()
            .all(|entry| entry.name != "decorate"),
        "{api:?}"
    );
    assert_eq!(
        api.import_rebindings
            .iter()
            .find(|entry| entry.name == "decorate")
            .unwrap()
            .equivalence,
        RebindingEquivalence::Unverified
    );
}

#[test]
fn python_import_rebinding_proves_literal_parameter_body_and_keeps_same_file_uses() {
    for import in [
        "from shared import decorate as decorate",
        "from shared import decorate",
    ] {
        let api = rebind(
            &format!("{import}\n\ndef own(value: str) -> str:\n    return decorate(value)\n"),
            FUNCTION,
            CLIENT,
        );
        assert!(
            api.removed.iter().all(|entry| entry.name != "decorate"),
            "{api:?}"
        );
        assert!(
            api.modified.iter().all(|entry| entry.name != "decorate"),
            "{api:?}"
        );
        assert!(
            api.compatible_modified
                .iter()
                .any(|entry| entry.name == "decorate" && entry.reason == "python_import_rebinding"),
            "{api:?}"
        );
        let proof = api
            .import_rebindings
            .iter()
            .find(|entry| entry.name == "decorate")
            .unwrap();
        assert_eq!(proof.equivalence, RebindingEquivalence::Proven);
        assert_eq!(proof.target.file, "shared.py");
        assert_eq!(proof.target.name, "decorate");
        assert_eq!(proof.target.line, 0);
        assert_eq!(proof.same_file_uses.len(), 1);
        assert_eq!(proof.same_file_uses[0].line, 3);
    }
}

#[test]
fn python_import_rebinding_preserved_name_alone_keeps_modified_blocking() {
    for (label, shared, client) in [
        (
            "changed contract",
            "def decorate(value: str, suffix: str) -> str:\n    return '<' + value + suffix\n",
            CLIENT,
        ),
        (
            "changed body",
            "def decorate(value: str) -> str:\n    return '[' + value + ']'\n",
            CLIENT,
        ),
        (
            "module global",
            "PREFIX = '<'\ndef decorate(value: str) -> str:\n    return PREFIX + value + '>'\n",
            CLIENT,
        ),
        (
            "function value",
            FUNCTION,
            "from api import decorate\ncallback = decorate\n",
        ),
        (
            "introspection",
            FUNCTION,
            "from api import decorate\nprint(decorate.__module__)\n",
        ),
        (
            "indirect frame introspection",
            FUNCTION,
            "from api import decorate\ndef discover(error):\n    return error.__traceback__.tb_frame.f_globals['decorate'].__module__\n",
        ),
        (
            "caller alias",
            FUNCTION,
            "from api import decorate as callback\nprint(callback('x'))\n",
        ),
    ] {
        let api = rebind("from shared import decorate as decorate\n", shared, client);
        assert!(
            api.removed.iter().all(|entry| entry.name != "decorate"),
            "{label}: {api:?}"
        );
        assert!(
            api.modified.iter().any(|entry| entry.name == "decorate"),
            "{label}: {api:?}"
        );
        assert!(
            api.compatible_modified
                .iter()
                .all(|entry| entry.name != "decorate"),
            "{label}: {api:?}"
        );
        let proof = api
            .import_rebindings
            .iter()
            .find(|entry| entry.name == "decorate")
            .unwrap();
        assert_eq!(
            proof.equivalence,
            RebindingEquivalence::Unverified,
            "{label}: {api:?}"
        );
    }
}

#[test]
fn python_import_rebinding_keeps_true_conditional_local_and_cyclic_removals_blocking() {
    for (label, replacement, shared) in [
        ("true removal", "", FUNCTION),
        (
            "conditional import",
            "if True:\n    from shared import decorate as decorate\n",
            FUNCTION,
        ),
        (
            "function local import",
            "def load():\n    from shared import decorate as decorate\n",
            FUNCTION,
        ),
        (
            "wrong public name",
            "from shared import decorate as renamed\n",
            FUNCTION,
        ),
        (
            "cycle",
            "from shared import decorate as decorate\n",
            "from api import decorate\ndef decorate(value: str) -> str:\n    return '<' + value + '>'\n",
        ),
        (
            "dynamic binding",
            "from shared import decorate as decorate\nexec('decorate = None')\n",
            FUNCTION,
        ),
        (
            "import side effect",
            "from shared import decorate as decorate\n",
            "print('imported')\ndef decorate(value: str) -> str:\n    return '<' + value + '>'\n",
        ),
        (
            "decorator",
            "from shared import decorate as decorate\n",
            "@wrap\ndef decorate(value: str) -> str:\n    return '<' + value + '>'\n",
        ),
    ] {
        let api = rebind(replacement, shared, CLIENT);
        assert!(
            api.removed.iter().any(|entry| entry.name == "decorate"),
            "{label}: {api:?}"
        );
        assert!(
            api.import_rebindings
                .iter()
                .all(|entry| entry.name != "decorate"),
            "{label}: {api:?}"
        );
    }
}
