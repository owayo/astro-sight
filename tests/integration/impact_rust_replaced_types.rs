//! 非公開の型の行を別の名前の型に置き換えた差分の impact (Issue #298)。

use super::support::*;

const CARGO: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

/// `files` をコミットし、`changes` に書き換えたときの context の出力。
fn context(files: &[(&str, &str)], changes: &[(&str, &str)]) -> serde_json::Value {
    let repo = TestRepo::new();
    repo.create_dir_all("src/internal");
    repo.write("Cargo.toml", CARGO);
    for (path, content) in files {
        repo.write(path, content);
    }
    repo.init_git();
    repo.commit_all("initial");
    for (path, content) in changes {
        repo.write(path, content);
    }
    repo.run_json("context", &["--git"])
}

/// `path` を参照する影響 (impacted_callers と low_confidence_callers) があるか。
fn has_caller(context: &serde_json::Value, path: &str) -> bool {
    context["changes"].as_array().unwrap().iter().any(|change| {
        ["impacted_callers", "low_confidence_callers"]
            .iter()
            .any(|bucket| {
                change[*bucket]
                    .as_array()
                    .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == path))
            })
    })
}

/// `file` の `name` の変更の種類。
fn change_type<'a>(context: &'a serde_json::Value, file: &str, name: &str) -> Option<&'a str> {
    context["changes"]
        .as_array()?
        .iter()
        .find(|change| change["path"] == file)?["affected_symbols"]
        .as_array()?
        .iter()
        .find(|symbol| symbol["name"] == name)?["change_type"]
        .as_str()
}

const LIB: &str = "pub mod kinds;\npub mod paths;\npub mod user;\nmod internal;\n";
const KINDS: &str = "pub enum RefKind {\n    Definition,\n    Use,\n}\n";
const USER: &str = "use crate::kinds::RefKind;\n\npub fn is_definition(kind: &RefKind) -> bool {\n    matches!(kind, RefKind::Definition)\n}\n";
const PATHS: &str = "enum ModulePath {\n    Crate,\n}\n\npub fn root() -> bool {\n    matches!(module(), ModulePath::Crate)\n}\n\nfn module() -> ModulePath {\n    ModulePath::Crate\n}\n";

/// Issue の再現: 非公開の型の行を別の名前の非公開の型に置き換えると、新しい型は旧い版に同じ名前の
/// 宣言が無いので added で、ほかのファイルの同じ名前の別のもの (別の enum の variant、別の module が
/// 自分で宣言した型) を影響にしない。
#[test]
fn impact_rust_replaced_types_are_added() {
    let files = [
        ("src/lib.rs", LIB),
        ("src/kinds.rs", KINDS),
        ("src/user.rs", USER),
        ("src/paths.rs", PATHS),
        (
            "src/internal.rs",
            "pub fn helper() {}\n\nstruct Alias {\n    tokens: Vec<String>,\n}\n\nenum Resolved {\n    Module(Vec<String>),\n}\n",
        ),
    ];
    let context = context(
        &files,
        &[(
            "src/internal.rs",
            "pub fn helper() {}\n\nstruct Definition {\n    range: (usize, usize),\n}\n\nenum ModulePath {\n    Inline(Vec<String>),\n}\n",
        )],
    );
    for name in ["Definition", "ModulePath"] {
        assert_eq!(
            change_type(&context, "src/internal.rs", name),
            Some("added"),
            "{name}: {context}"
        );
    }
    assert!(!has_caller(&context, "src/user.rs"), "{context}");
    assert!(!has_caller(&context, "src/paths.rs"), "{context}");
}

/// 正しい影響は残す: 見出しを変えた `pub(crate)` の型の利用と、非公開の型の別のファイルの子
/// module からの利用。
#[test]
fn impact_rust_replaced_types_keep_real_callers() {
    // `pub(crate)` の型に型引数を足す
    let context_crate = context(
        &[
            ("src/lib.rs", "mod internal;\npub mod consumer;\n"),
            (
                "src/internal.rs",
                "pub(crate) struct Holder {\n    pub(crate) value: u8,\n}\n",
            ),
            (
                "src/consumer.rs",
                "use crate::internal::Holder;\n\npub(crate) fn read(holder: &Holder) -> u8 {\n    holder.value\n}\n",
            ),
        ],
        &[(
            "src/internal.rs",
            "pub(crate) struct Holder<T> {\n    pub(crate) value: T,\n}\n",
        )],
    );
    assert!(
        has_caller(&context_crate, "src/consumer.rs"),
        "{context_crate}"
    );
    // 非公開の型に型引数を足す (別のファイルの子 module からは名指せる)
    let context_private = context(
        &[
            ("src/lib.rs", "mod internal;\n"),
            (
                "src/internal.rs",
                "mod child;\n\nstruct Holder {\n    value: u8,\n}\n",
            ),
            (
                "src/internal/child.rs",
                "use super::Holder;\n\npub(crate) fn read(holder: &Holder) -> u8 {\n    holder.value\n}\n",
            ),
        ],
        &[(
            "src/internal.rs",
            "mod child;\n\nstruct Holder<T> {\n    value: T,\n}\n",
        )],
    );
    assert!(
        has_caller(&context_private, "src/internal/child.rs"),
        "{context_private}"
    );
}
