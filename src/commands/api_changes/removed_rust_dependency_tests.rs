use super::tests::site_in;
use super::*;

/// workspace の alpha が path 依存の beta を持ち、beta の `items` が `run` を宣言する。
fn dependency_workspace<'a>(
    alpha_lib: &'a str,
    alpha_dependencies: &'a str,
) -> Vec<(&'a str, String)> {
    vec![
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n".to_owned(),
        ),
        (
            "crates/alpha/Cargo.toml",
            format!(
                "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{alpha_dependencies}"
            ),
        ),
        ("crates/alpha/src/lib.rs", alpha_lib.to_owned()),
        (
            "crates/beta/Cargo.toml",
            "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_owned(),
        ),
        ("crates/beta/src/lib.rs", "pub mod items;\n".to_owned()),
        ("crates/beta/src/items.rs", "pub fn run() {}\n".to_owned()),
    ]
}

fn dependency_site(
    alpha_lib: &str,
    alpha_dependencies: &str,
    extra: &[(&str, &str)],
    ref_path: &str,
) -> Option<PathSite> {
    let base = dependency_workspace(alpha_lib, alpha_dependencies);
    let mut files: Vec<(&str, &str)> = base
        .iter()
        .map(|(path, source)| (*path, source.as_str()))
        .collect();
    files.extend_from_slice(extra);
    site_in(&files, ref_path)
}

/// alpha の lib.rs、alpha の依存の表、変えるほかのファイル、参照ファイル。
type DependencyCase<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], &'a str);

const ALPHA_LIB: &str = "crates/alpha/src/lib.rs";
const NORMAL_BETA: &str = "[dependencies]\nbeta = { path = \"../beta\" }\n";
const CALL_BETA: &str = "pub fn f() { beta::items::run(); }\n";

#[test]
fn dependency_paths_keep_conditional_dependency_roots() {
    for attribute in [
        "#![cfg(any())]",
        "#![cfg_attr(all(), cfg(any()))]",
        "#![cfg_attr(any(), cfg(any()))]",
        "#![cfg_attr(all(), cfg_attr(all(), cfg(any())))]",
    ] {
        for (call, declaration) in [
            (CALL_BETA, "pub mod items;"),
            ("pub fn f() { ::beta::run(); }", "pub fn run() {}"),
        ] {
            let root = format!("{attribute}\n{declaration}\n");
            assert_eq!(
                dependency_site(
                    call,
                    NORMAL_BETA,
                    &[("crates/beta/src/lib.rs", &root)],
                    ALPHA_LIB,
                ),
                None,
                "{root}{call}"
            );
        }
    }
    for attribute in [
        "#![allow(dead_code)]",
        "#![cfg_attr(all(), allow(dead_code), warn(unused))]",
    ] {
        let root = format!("{attribute}\npub mod items;\n");
        assert_eq!(
            dependency_site(
                CALL_BETA,
                NORMAL_BETA,
                &[("crates/beta/src/lib.rs", &root)],
                ALPHA_LIB,
            ),
            Some(PathSite::ModuleFunction(Rc::from(
                "crates/beta/src/items.rs"
            ))),
            "{root}"
        );
    }
    // 同じ crate 全体が消える条件は、110 の同 crate 参照の証明を変えない。
    assert_eq!(
        dependency_site(
            "#![cfg(any())]\npub mod items;\npub fn f() { crate::items::run(); }\n",
            NORMAL_BETA,
            &[("crates/alpha/src/items.rs", "pub fn run() {}\n")],
            ALPHA_LIB,
        ),
        Some(PathSite::ModuleFunction(Rc::from(
            "crates/alpha/src/items.rs"
        )))
    );
}

/// 依存クレートの名前で始まるパスは、依存先の残る直接宣言へ解決できたときだけ証明する
/// (Issue #115)。
#[test]
fn removed_rust_paths_resolve_dependency_crates() {
    let beta = Some(PathSite::ModuleFunction(Rc::from(
        "crates/beta/src/items.rs",
    )));
    let cases: &[DependencyCase<'_>] = &[
        (CALL_BETA, NORMAL_BETA, &[], ALPHA_LIB),
        // ローカルに beta があっても `::beta` は外部クレート
        (
            "mod beta {}\npub fn f() { ::beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            "#![no_implicit_prelude]\npub fn f() { ::beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        // 置き換えていない組み込みの derive は名前を足さない
        (
            "#[derive(Debug, Clone)]\npub struct S;\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        // 改名 (`-` は `_`)・キーと同じ名前の package・依存先の lib 名
        (
            "pub fn f() { beta_core::items::run(); }\n",
            "[dependencies]\nbeta-core = { path = \"../beta\", package = \"beta\" }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dependencies]\nbeta = { path = \"../beta\", package = \"beta\" }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            "pub fn f() { beta_lib::items::run(); }\n",
            NORMAL_BETA,
            &[(
                "crates/beta/Cargo.toml",
                "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"beta_lib\"\n",
            )],
            ALPHA_LIB,
        ),
        // 同じ依存先を指す dev の表・workspace の依存・依存を変えない設定・root 以外のモジュール
        (
            CALL_BETA,
            "[dependencies]\nbeta = { path = \"../beta\" }\n\n[dev-dependencies]\nbeta = { path = \"../beta\" }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dependencies]\nbeta = { workspace = true }\n",
            &[(
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n\n[workspace.dependencies]\nbeta = { path = \"crates/beta\" }\n",
            )],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(".cargo/config.toml", "[env]\nSAMPLE = \"1\"\n")],
            ALPHA_LIB,
        ),
        (
            "pub mod caller;\n",
            NORMAL_BETA,
            &[("crates/alpha/src/caller.rs", CALL_BETA)],
            "crates/alpha/src/caller.rs",
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[("crates/alpha/tests/it.rs", CALL_BETA)],
            "crates/alpha/tests/it.rs",
        ),
    ];
    for (alpha_lib, dependencies, extra, ref_path) in cases {
        assert_eq!(
            dependency_site(alpha_lib, dependencies, extra, ref_path),
            beta,
            "{alpha_lib}{dependencies}{extra:?}"
        );
    }
    // ローカルの mod beta があれば、外部クレートではなくそのモジュールを解決する
    assert_eq!(
        dependency_site(
            "mod beta { pub mod items { pub fn run() {} } }\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        Some(PathSite::ModuleFunction(Rc::from(ALPHA_LIB)))
    );
}

/// 名前を生成し得るものや、依存先を確かめられない依存の形では、外部クレートへ解決しない。
#[test]
fn removed_rust_paths_keep_unproven_dependency_crates() {
    let cases: &[DependencyCase<'_>] = &[
        // M の名前: マクロ・custom derive・置き換えた組み込み derive・グロブ・ローカルの空の mod
        (
            "macro_rules! bind { () => { use crate as beta; } }\nbind!();\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            "#[derive(Inject)]\npub struct S;\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            "use other::Debug;\n#[derive(Debug)]\npub struct S;\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            "#[cfg_attr(test, derive(Debug))]\npub struct S;\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            "use std::fmt::*;\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            "mod beta {}\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            "#![no_implicit_prelude]\npub fn f() { beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        // crate root の extern crate は extern prelude を変える
        (
            "extern crate self as beta;\npub mod caller;\n",
            NORMAL_BETA,
            &[("crates/alpha/src/caller.rs", CALL_BETA)],
            "crates/alpha/src/caller.rs",
        ),
        // 依存の表: 無い・自分の名前・build / dev / target / optional だけ・別の依存先・registry
        (CALL_BETA, "", &[], ALPHA_LIB),
        (
            "pub fn f() { alpha::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[build-dependencies]\nbeta = { path = \"../beta\" }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dev-dependencies]\nbeta = { path = \"../beta\" }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[target.'cfg(unix)'.dependencies]\nbeta = { path = \"../beta\" }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dependencies]\nbeta = { path = \"../beta\", optional = true }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dependencies]\nbeta = { path = \"../beta\" }\n\n[target.'cfg(unix)'.dependencies]\nbeta = { path = \"../gamma\", package = \"gamma\" }\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dependencies]\nbeta = { path = \"../beta\" }\n\n[dev-dependencies]\nbeta = \"0.1\"\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dependencies]\nbeta = \"0.1\"\n",
            &[],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            "[dependencies]\nbeta = { path = \"../../../beta\" }\n",
            &[],
            ALPHA_LIB,
        ),
        // 依存先: 条件付きの fn と mod・再公開・lib として使えない package
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(
                "crates/beta/src/items.rs",
                "#[cfg(feature = \"x\")]\npub fn run() {}\n",
            )],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(
                "crates/beta/src/lib.rs",
                "#[cfg(feature = \"x\")]\npub mod items;\n",
            )],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[("crates/beta/src/items.rs", "pub use other::run;\n")],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(
                "crates/beta/Cargo.toml",
                "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
            )],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(
                "crates/beta/Cargo.toml",
                "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\nautolib = false\n",
            )],
            ALPHA_LIB,
        ),
        // 依存先を差し替え得る patch と Cargo の設定、対象外の target の中の参照
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n\n[patch.crates-io]\nother = { path = \"crates/beta\" }\n",
            )],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(".cargo/config.toml", "paths = [\"crates/alpha\"]\n")],
            ALPHA_LIB,
        ),
        (
            CALL_BETA,
            NORMAL_BETA,
            &[(
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--extern\", \"beta=x\"]\n",
            )],
            ALPHA_LIB,
        ),
    ];
    for (alpha_lib, dependencies, extra, ref_path) in cases {
        assert_eq!(
            dependency_site(alpha_lib, dependencies, extra, ref_path),
            None,
            "{alpha_lib}{dependencies}{extra:?}"
        );
    }
}

#[test]
fn dependency_paths_keep_generated_root_extern_aliases() {
    let bind = "macro_rules! bind { () => { extern crate self as beta; } }\nbind!();\n";
    for call in [CALL_BETA, "pub fn f() { ::beta::items::run(); }\n"] {
        let root = format!("{bind}pub mod caller;\n");
        assert_eq!(
            dependency_site(
                &root,
                NORMAL_BETA,
                &[("crates/alpha/src/caller.rs", call)],
                "crates/alpha/src/caller.rs"
            ),
            None,
            "{call}"
        );
    }
    let root = format!("{bind}pub fn f() {{ ::beta::items::run(); }}\n");
    assert_eq!(dependency_site(&root, NORMAL_BETA, &[], ALPHA_LIB), None);
    for attribute in [
        "#[derive(Inject)]",
        "#[inject]",
        "#[cfg_attr(test, derive(Debug))]",
    ] {
        let root = format!("{attribute}\npub struct S;\npub mod caller;\n");
        assert_eq!(
            dependency_site(
                &root,
                NORMAL_BETA,
                &[("crates/alpha/src/caller.rs", CALL_BETA)],
                "crates/alpha/src/caller.rs"
            ),
            None,
            "{attribute}"
        );
    }
    assert_eq!(
        dependency_site(
            "use macros::*;\n#[derive(Debug)]\npub struct S;\npub fn f() { ::beta::items::run(); }\n",
            NORMAL_BETA,
            &[],
            ALPHA_LIB
        ),
        None
    );
}

#[test]
fn dependency_paths_reject_malformed_dependency_fields() {
    for spec in [
        "{ path = \"../beta\", optional = \"true\" }",
        "{ path = \"../beta\", package = 1 }",
        "{ path = \"../beta\", package = \"different\" }",
        "{ path = \"../beta\", git = \"https://example.invalid/beta\" }",
        "{ path = \"../beta\", workspace = false }",
        "{ path = \"../beta\", features = true }",
        "{ path = \"../beta\", artifact = \"bin\" }",
        "{ path = \"../beta\", lib = false }",
    ] {
        let dependencies = format!("[dependencies]\nbeta = {spec}\n");
        assert_eq!(
            dependency_site(CALL_BETA, &dependencies, &[], ALPHA_LIB),
            None,
            "{spec}"
        );
    }
}

fn package_with_lib(name: &str, library: &str, dependencies: &str, build: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{build}\n[lib]\npath = \"{library}\"\n\n[dependencies]\n{dependencies}"
    )
}

#[test]
fn dependency_paths_require_matching_targets_for_every_membership() {
    let alpha = package_with_lib(
        "alpha",
        "src/shared.rs",
        "beta = { path = \"../beta\" }",
        "",
    );
    for (gamma_dependencies, same) in [
        ("beta = { path = \"../beta\" }", true),
        ("", false),
        ("beta = { path = \"../other\", package = \"other\" }", false),
    ] {
        let gamma = package_with_lib("gamma", "../alpha/src/shared.rs", gamma_dependencies, "");
        let other = package_with_lib("other", "src/lib.rs", "", "");
        let extra = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/gamma\", \"crates/other\"]\n",
            ),
            ("crates/alpha/Cargo.toml", alpha.as_str()),
            ("crates/alpha/src/shared.rs", CALL_BETA),
            ("crates/gamma/Cargo.toml", gamma.as_str()),
            ("crates/other/Cargo.toml", other.as_str()),
            ("crates/other/src/lib.rs", "pub mod items;\n"),
            ("crates/other/src/items.rs", "pub fn run() {}\n"),
        ];
        let expected = same.then(|| PathSite::ModuleFunction(Rc::from("crates/beta/src/items.rs")));
        assert_eq!(
            dependency_site("", NORMAL_BETA, &extra, "crates/alpha/src/shared.rs"),
            expected,
            "{gamma_dependencies}"
        );
    }
}

#[test]
fn dependency_paths_keep_build_script_memberships() {
    let call = format!("{CALL_BETA}fn main() {{}}\n");
    for (build, is_library_only) in [("build = false", true), ("", false)] {
        let manifest = package_with_lib(
            "alpha",
            "build.rs",
            "beta = { path = \"../beta\" }\n[build-dependencies]\nbeta = { path = \"../beta\" }",
            build,
        );
        let expected =
            is_library_only.then(|| PathSite::ModuleFunction(Rc::from("crates/beta/src/items.rs")));
        assert_eq!(
            dependency_site(
                "",
                NORMAL_BETA,
                &[
                    ("crates/alpha/Cargo.toml", &manifest),
                    ("crates/alpha/build.rs", &call)
                ],
                "crates/alpha/build.rs"
            ),
            expected,
            "{build}"
        );
    }
}

#[test]
fn dependency_paths_follow_explicit_library_roots() {
    for package_settings in ["", "autolib = false"] {
        let manifest = package_with_lib("beta", "core.rs", "", package_settings);
        assert_eq!(
            dependency_site(
                CALL_BETA,
                NORMAL_BETA,
                &[
                    ("crates/beta/Cargo.toml", &manifest),
                    ("crates/beta/core.rs", "pub mod items;\n"),
                    ("crates/beta/items.rs", "pub fn run() {}\n"),
                ],
                ALPHA_LIB
            ),
            Some(PathSite::ModuleFunction(Rc::from("crates/beta/items.rs")))
        );
    }
}

#[test]
fn dependency_paths_resolve_workspace_renames_and_explicit_workspace() {
    let alpha = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\nworkspace = \"../..\"\n[dependencies]\nshared = { workspace = true }\n";
    let workspace = "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n[workspace.dependencies]\nshared = { path = \"crates/beta\", package = \"beta\" }\n";
    assert_eq!(
        dependency_site(
            "pub fn f() { shared::items::run(); }\n",
            "",
            &[
                ("crates/alpha/Cargo.toml", alpha),
                ("Cargo.toml", workspace),
            ],
            ALPHA_LIB
        ),
        Some(PathSite::ModuleFunction(Rc::from(
            "crates/beta/src/items.rs"
        )))
    );
}

#[test]
fn dependency_paths_compare_every_dependency_table() {
    let gamma = package_with_lib("gamma", "src/lib.rs", "", "");
    for (target, same) in [
        ("beta = { path = \"../beta\" }", true),
        ("beta = { path = \"../gamma\", package = \"gamma\" }", false),
    ] {
        let dependencies = format!("{NORMAL_BETA}\n[target.'cfg(unix)'.dependencies]\n{target}\n");
        let expected = same.then(|| PathSite::ModuleFunction(Rc::from("crates/beta/src/items.rs")));
        assert_eq!(
            dependency_site(
                CALL_BETA,
                &dependencies,
                &[
                    (
                        "Cargo.toml",
                        "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/gamma\"]\n"
                    ),
                    ("crates/gamma/Cargo.toml", &gamma),
                    ("crates/gamma/src/lib.rs", "pub mod items;\n"),
                    ("crates/gamma/src/items.rs", "pub fn run() {}\n"),
                ],
                ALPHA_LIB
            ),
            expected,
            "{target}"
        );
    }
}

#[test]
fn dependency_paths_keep_nfc_equivalent_local_type_names() {
    let dependencies = "[dependencies]\nK = { path = \"../beta\", package = \"beta\" }\n";
    assert_eq!(
        dependency_site(
            "pub mod \u{212a} { pub mod items {} }\npub fn f() { K::items::run(); }\n",
            dependencies,
            &[],
            ALPHA_LIB
        ),
        None
    );
    assert_eq!(
        dependency_site(
            "pub mod \u{212a} { pub mod items {} }\npub fn f() { ::K::items::run(); }\n",
            dependencies,
            &[],
            ALPHA_LIB
        ),
        Some(PathSite::ModuleFunction(Rc::from(
            "crates/beta/src/items.rs"
        )))
    );
    assert_eq!(
        dependency_site(
            "pub mod \u{212a} { pub mod items { pub fn run() {} } }\npub fn f() { \u{212a}::items::run(); }\n",
            dependencies,
            &[],
            ALPHA_LIB
        ),
        Some(PathSite::ModuleFunction(Rc::from(ALPHA_LIB)))
    );
    assert_eq!(
        dependency_site(
            "pub struct \u{212a};\npub fn f() { \u{212a}::run(); }\n",
            dependencies,
            &[],
            ALPHA_LIB
        ),
        Some(PathSite::Associated)
    );
}

#[cfg(windows)]
#[test]
fn dependency_paths_use_audited_case_aliases() {
    let probe = tempfile::tempdir().unwrap();
    std::fs::write(probe.path().join("case-probe"), "same file").unwrap();
    if !probe.path().join("CASE-PROBE").is_file() {
        return;
    }
    assert_eq!(
        dependency_site(
            CALL_BETA,
            "[dependencies]\nbeta = { path = \"../BETA\" }\n",
            &[("Cargo.toml", "[workspace]\nmembers = [\"crates/alpha\"]\n")],
            ALPHA_LIB
        ),
        Some(PathSite::ModuleFunction(Rc::from(
            "crates/beta/src/items.rs"
        )))
    );
}

#[test]
fn dependency_paths_do_not_fold_unaudited_package_spellings() {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in dependency_workspace(CALL_BETA, NORMAL_BETA) {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let layout = layout::Layout::build(dir.path()).unwrap();
    assert_eq!(
        layout.package_at("crates/alpha", "../beta"),
        Some(Rc::from("crates/beta"))
    );
    assert_eq!(layout.package_at("crates/alpha", "../BETA"), None);
}
