use crate::commands::tests::common::*;
use crate::models::review::{ApiChanges, ApiModificationUnverifiedReason as Reason};

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[features]\nwide = []\nother = []\n";

fn compare(old: &str, new: &str) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo_for_test(dir.path());
    git_commit_files(
        dir.path(),
        &[("Cargo.toml", CARGO), ("src/lib.rs", old)],
        "base",
    );
    std::fs::write(dir.path().join("src/lib.rs"), new).unwrap();
    detect_api_changes_from_worktree(dir.path())
}

fn run_changed(api: &ApiChanges) -> bool {
    api.modified.iter().any(|change| {
        change.name == "run"
            && change
                .unverified
                .as_ref()
                .is_some_and(|why| why.reason == Reason::RustSignatureDependencyChanged)
    })
}

fn cases() -> [(&'static str, &'static str); 5] {
    [
        (
            "type Base = u8; mod fallback { pub type Text = super::Base; } use fallback::*; #[cfg(feature = \"wide\")] type Text = u32;",
            "Text",
        ),
        (
            "mod fallback { pub type Text = u8; } mod selected { pub type Text = u32; } use fallback::*; #[cfg(feature = \"wide\")] use selected::Text;",
            "Text",
        ),
        (
            "mod fallback { pub type Raw = u8; } use fallback::*; #[cfg(feature = \"wide\")] type Raw = u32; use Raw as Out;",
            "Out",
        ),
        (
            "#[cfg(feature = \"wide\")] mod selected { pub type T = u32; } mod fallback { pub mod selected { pub type T = u8; } } use fallback::*;",
            "selected::T",
        ),
        (
            "#[cfg(feature = \"wide\")] mod selected { pub type T = u32; } mod fallback { pub mod selected { pub type T = u8; } } use fallback::*; use selected as chosen;",
            "chosen::T",
        ),
    ]
}

#[test]
fn conditional_glob_fallback_tracks_dependencies_outside_explicit_bindings() {
    for (prefix, ty) in cases() {
        let old = format!("{prefix}\npub fn run(_: {ty}) {{}}\n");
        let api = compare(&old, &old.replace("u8", "u16"));
        assert!(run_changed(&api), "{ty}: {api:?}");
    }
}

#[test]
fn conditional_glob_fallback_does_not_compare_hidden_branches() {
    for (prefix, ty) in cases() {
        let old = format!("{prefix}\n#[cfg(feature = \"wide\")] pub fn run(_: {ty}) {{}}\n");
        let api = compare(&old, &old.replace("u8", "u16"));
        assert!(
            api.modified.iter().all(|change| change.name != "run"),
            "{ty}: {api:?}"
        );
    }
}

#[test]
fn conditional_glob_fallback_combines_cfg_attr_and_current_use_conditions() {
    for (prefix, ty, guard, expected) in [
        (
            "mod fallback { pub type Text = u8; } use fallback::*; #[cfg_attr(not(feature = \"wide\"), cfg(any()))] type Text = u32;",
            "Text",
            "",
            true,
        ),
        (
            "mod fallback { pub type Text = u8; } use fallback::*; #[cfg_attr(not(feature = \"wide\"), cfg(any()))] type Text = u32;",
            "Text",
            "#[cfg(feature = \"wide\")]",
            false,
        ),
        (
            "mod fallback { pub type Raw = u8; } use fallback::*; #[cfg(feature = \"wide\")] type Raw = u32; #[cfg(feature = \"wide\")] use Raw as Out;",
            "Out",
            "",
            false,
        ),
        (
            "mod fallback { pub type Text = u8; } #[cfg(feature = \"wide\")] use fallback::*; #[cfg(feature = \"wide\")] type Text = u32;",
            "Text",
            "",
            false,
        ),
        (
            "mod fallback { pub type Text = u8; } use fallback::*; #[cfg(feature = \"wide\")] type Text = u32; #[cfg(not(feature = \"wide\"))] type Text = u64;",
            "Text",
            "",
            false,
        ),
        (
            "mod fallback { pub type Text = u8; } use fallback::*; #[cfg(feature = \"wide\")] type Text = u32;",
            "Text",
            "#[cfg(any(feature = \"wide\", feature = \"other\"))]",
            true,
        ),
    ] {
        let old = format!("{prefix}\n{guard} pub fn run(_: {ty}) {{}}\n");
        let api = compare(&old, &old.replace("u8", "u16"));
        if expected {
            assert!(run_changed(&api), "{prefix}/{guard}: {api:?}");
        } else {
            assert!(
                api.modified.iter().all(|change| change.name != "run"),
                "{prefix}/{guard}: {api:?}"
            );
        }
    }
}
