use crate::commands::tests::common::*;
use crate::models::review::{ApiChanges, ApiModificationUnverifiedReason};
use std::fs;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const TYPES: &str = "pub mod a { pub struct S; }\npub mod b { pub struct S; }\n";

fn after_change(old: &str, new: &str, methods: Option<&str>) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("Cargo.toml", CARGO), ("src/lib.rs", old)], "base");
    fs::write(repo.join("src/lib.rs"), new).unwrap();
    if let Some(methods) = methods {
        fs::write(repo.join("src/methods.rs"), methods).unwrap();
    }
    detect_api_changes_from_worktree(repo)
}

fn assert_owner_blocking(api: &ApiChanges) {
    assert!(
        api.modified.iter().any(|change| {
            change.name == "f"
                && change.unverified.as_ref().is_some_and(|unverified| {
                    matches!(
                        unverified.reason,
                        ApiModificationUnverifiedReason::RustSignatureDependencyChanged
                            | ApiModificationUnverifiedReason::RustSignatureDependencyUnresolved
                    )
                })
        }),
        "{api:?}"
    );
    assert!(
        !api.moved.iter().any(|symbol| symbol.name == "f"),
        "{api:?}"
    );
}

#[test]
fn qualified_impl_move_to_another_nominal_owner_is_blocking() {
    let old = format!("{TYPES}impl a::S {{ pub fn f(&self) {{}} }}\n");
    let new = old.replace("impl a::S", "impl b::S");
    assert_owner_blocking(&after_change(&old, &new, None));
    assert_owner_blocking(&after_change(
        &old,
        &format!("{TYPES}pub mod methods;\n"),
        Some("impl crate::b::S { pub fn f(&self) {} }\n"),
    ));
}

#[test]
fn qualified_impl_move_keeps_the_same_resolved_owner() {
    let old = format!("{TYPES}impl a::S {{ pub fn f(&self) {{}} }}\n");
    for (new, methods) in [
        (old.replace("impl a::S", "impl crate::a::S"), None),
        (
            format!("{TYPES}pub mod methods;\n"),
            Some("impl crate::a::S { pub fn f(&self) {} }\n"),
        ),
        (
            format!(
                "{TYPES}use crate::a as selected;\nimpl selected::S {{ pub fn f(&self) {{}} }}\n"
            ),
            None,
        ),
    ] {
        let api = after_change(&old, &new, methods);
        assert!(
            api.modified.iter().all(|change| change.name != "f"),
            "{api:?}"
        );
        assert!(api.moved.iter().any(|symbol| symbol.name == "f"), "{api:?}");
    }
}

#[test]
fn qualified_impl_move_still_checks_type_arguments_and_bounds() {
    let types = "pub mod a { pub struct S<T>(pub T); }\n";
    for (old_header, new_header) in [
        ("impl a::S<u8>", "impl crate::a::S<u16>"),
        ("impl<T> a::S<T>", "impl<T: Copy> crate::a::S<T>"),
        ("impl<T> a::S<T>", "impl<T> crate::a::S<T> where T: Copy"),
    ] {
        let old = format!("{types}{old_header} {{ pub fn f(&self) {{}} }}\n");
        let new = format!("{types}{new_header} {{ pub fn f(&self) {{}} }}\n");
        let api = after_change(&old, &new, None);
        let change = api
            .modified
            .iter()
            .find(|change| change.name == "f")
            .unwrap_or_else(|| panic!("{old_header} -> {new_header}: {api:?}"));
        // guard の実定義の照合を通っても、後段が impl の型引数と境界の違いを報告する。
        assert!(
            change
                .old_signature
                .as_deref()
                .is_some_and(|signature| signature.starts_with("impl")),
            "{change:?}"
        );
        assert!(
            change
                .new_signature
                .as_deref()
                .is_some_and(|signature| signature.starts_with("impl")),
            "{change:?}"
        );
    }
}

#[test]
fn qualified_impl_move_unknown_or_conditional_owner_stays_blocking() {
    for (old, new) in [
        (
            "impl old::S { pub fn f(&self) {} }\n",
            "impl new::S { pub fn f(&self) {} }\n",
        ),
        (
            "pub mod a { pub struct S; }\n#[custom] impl a::S { pub fn f(&self) {} }\n",
            "pub mod a { pub struct S; }\n#[custom] impl crate::a::S { pub fn f(&self) {} }\n",
        ),
        (
            "pub mod a { pub struct S; }\n#[cfg(feature = \"first\")] use crate::a as selected;\nimpl selected::S { pub fn f(&self) {} }\n",
            "pub mod a { pub struct S; }\n#[cfg(feature = \"second\")] use crate::a as selected;\nimpl crate::a::S { pub fn f(&self) {} }\n",
        ),
    ] {
        assert_owner_blocking(&after_change(old, new, None));
    }
}

#[test]
fn qualified_impl_move_follows_a_uniquely_moved_owner_type() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", "pub mod a;\npub mod methods;\n"),
            ("src/a.rs", "pub struct S;\n"),
            (
                "src/methods.rs",
                "impl crate::a::S { pub fn f(&self) {} }\n",
            ),
        ],
        "base",
    );
    fs::write(repo.join("src/lib.rs"), "pub mod b;\npub mod methods;\n").unwrap();
    fs::remove_file(repo.join("src/a.rs")).unwrap();
    fs::write(repo.join("src/b.rs"), "pub struct S;\n").unwrap();
    fs::write(
        repo.join("src/methods.rs"),
        "impl crate::b::S { pub fn f(&self) {} }\n",
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.modified.iter().all(|change| change.name != "f"),
        "{api:?}"
    );
    assert!(api.moved.iter().any(|symbol| symbol.name == "f"), "{api:?}");
}

#[test]
fn qualified_trait_impls_do_not_create_inherent_api_changes() {
    let types =
        "pub mod a { pub struct S; }\npub mod b { pub struct S; }\npub trait Tr { fn f(&self); }\n";
    let old = format!("{types}impl Tr for a::S {{ fn f(&self) {{}} }}\n");
    let new = old.replace("impl Tr for a::S", "impl Tr for b::S");
    let api = after_change(&old, &new, None);
    assert!(api.modified.is_empty(), "{api:?}");
}

#[test]
fn qualified_impl_move_under_multiple_paths_requires_an_absolute_same_owner() {
    for (old_owner, new_owner, compatible) in [
        ("crate::S", "crate::S", true),
        ("crate::S", "crate::T", false),
        ("super::S", "super::S", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let old = format!("fn keep_a() {{}}\nimpl {old_owner} {{ pub fn f(&self) {{}} }}\n");
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", CARGO),
                (
                    "src/lib.rs",
                    "pub struct S;\npub struct T;\npub mod p;\npub mod q;\n",
                ),
                (
                    "src/p.rs",
                    "pub struct S;\n#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\npub mod a;\n#[cfg(not(feature = \"x\"))]\n#[path = \"b.rs\"]\npub mod b;\n",
                ),
                (
                    "src/q.rs",
                    "pub struct S;\n#[cfg(not(feature = \"x\"))]\n#[path = \"a.rs\"]\npub mod a;\n#[cfg(feature = \"x\")]\n#[path = \"b.rs\"]\npub mod b;\n",
                ),
                ("src/a.rs", &old),
                ("src/b.rs", "fn keep_b() {}\n"),
            ],
            "base",
        );
        fs::write(repo.join("src/a.rs"), "fn keep_a() {}\n").unwrap();
        fs::write(
            repo.join("src/b.rs"),
            format!("fn keep_b() {{}}\nimpl {new_owner} {{ pub fn f(&self) {{}} }}\n"),
        )
        .unwrap();
        let api = detect_api_changes_from_worktree(repo);
        if compatible {
            assert!(
                api.modified.iter().all(|change| change.name != "f"),
                "{api:?}"
            );
            assert!(api.moved.iter().any(|symbol| symbol.name == "f"), "{api:?}");
        } else {
            assert_owner_blocking(&api);
        }
    }
}
