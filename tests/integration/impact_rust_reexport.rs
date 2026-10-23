//! Rust references through imports and re-exports retain their declaration owner.

use super::support::*;

#[test]
fn impact_rust_imports_and_reexports_follow_their_owner() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin; pub mod other; pub mod bridge; pub mod obridge; pub mod bare; pub mod tbare; pub mod t_direct; pub mod t_bridge; pub mod t_bare; pub mod u_bridge; pub mod u_bare; pub mod u_named; pub mod u_brace; pub mod u_glob;\n#[cfg(test)] mod tests { use super::*; #[test] fn smoke() { assert_eq!(1, 1); } }\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write(
        "src/other.rs",
        "use std::cmp::Ordering;\n#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]\npub struct Payload { pub value: u8 }\n",
    );
    repo.write("src/bridge.rs", "pub use crate::origin::Payload;\n");
    repo.write("src/obridge.rs", "pub use crate::other::Payload;\n");
    repo.create_dir_all("src/bare");
    repo.create_dir_all("src/tbare");
    repo.write("src/bare.rs", "pub mod child; pub use child::Payload;\n");
    repo.write("src/bare/child.rs", "pub use crate::other::Payload;\n");
    repo.write("src/tbare.rs", "pub mod child; pub use child::Payload;\n");
    repo.write("src/tbare/child.rs", "pub use crate::origin::Payload;\n");
    repo.write(
        "src/t_direct.rs",
        "pub fn read(value: crate::origin::Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/t_bridge.rs",
        "pub fn read(value: crate::bridge::Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/t_bare.rs",
        "pub fn read(value: crate::tbare::Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/u_bridge.rs",
        "pub fn read(value: crate::obridge::Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/u_bare.rs",
        "pub fn read(value: crate::bare::Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/u_named.rs",
        "use std::fmt; use crate::other::Payload; pub fn read(value: Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/u_brace.rs",
        "use crate::other::{Payload}; pub fn read(value: Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/u_glob.rs",
        "use std::fmt; use crate::other::*; pub fn read(value: Payload) -> u8 { value.value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/origin.rs")
        .unwrap();
    for file in ["src/t_direct.rs", "src/t_bridge.rs", "src/t_bare.rs"] {
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == file)),
            "{file}: {context}"
        );
    }
    for file in [
        "src/u_bridge.rs",
        "src/u_bare.rs",
        "src/u_named.rs",
        "src/u_brace.rs",
        "src/u_glob.rs",
    ] {
        for bucket in [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers",
        ] {
            assert!(
                change[bucket]
                    .as_array()
                    .is_none_or(|callers| callers.iter().all(|caller| caller["path"] != file)),
                "{file} {bucket}: {context}"
            );
        }
    }

    repo.stage_all();
    let staged = repo.run_json("context", &["--git", "--staged"]);
    let staged_change = staged["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/origin.rs")
        .unwrap();
    assert!(
        [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers"
        ]
        .iter()
        .any(|bucket| staged_change[bucket]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|caller| caller["path"] == "src/u_named.rs"))),
        "staged analysis must keep unproved references: {staged}"
    );
}

#[test]
fn impact_rust_alias_import_line_represents_renamed_calls() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin; pub mod walk; pub mod type_user; pub mod function_user; pub mod unused; pub mod public_bridge; pub mod parent; pub mod private_parent; pub mod nested_parent; pub mod included; pub mod unknown;\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write("src/walk.rs", "pub fn collect(value: u8) -> u8 { value }\n");
    repo.write(
        "src/type_user.rs",
        "use crate::origin::Payload as Data; pub fn read(value: Data) -> u8 { value.value }\n",
    );
    repo.write(
        "src/function_user.rs",
        "use crate::walk::collect as gather; pub fn read() -> u8 { gather(1) }\n",
    );
    repo.write("src/unused.rs", "use crate::origin::Payload as Data;\n");
    repo.write(
        "src/public_bridge.rs",
        "pub use crate::origin::Payload as Data;\n",
    );
    repo.create_dir_all("src/parent");
    repo.write(
        "src/parent.rs",
        "pub mod child; pub fn read(value: child::Data) -> u8 { value.value }\n",
    );
    repo.write(
        "src/parent/child.rs",
        "pub(super) use crate::origin::Payload as Data;\n",
    );
    repo.create_dir_all("src/private_parent");
    repo.write(
        "src/private_parent.rs",
        "use crate::origin::Payload as Data; pub mod child;\n",
    );
    repo.write(
        "src/private_parent/child.rs",
        "pub fn read(value: super::Data) -> u8 { value.value }\n",
    );
    repo.create_dir_all("src/nested_parent/tests");
    repo.write(
        "src/nested_parent.rs",
        "use crate::origin::Payload as Data; mod tests { mod helpers; }\n",
    );
    repo.write(
        "src/nested_parent/tests/helpers.rs",
        "pub fn read(value: super::super::Data) -> u8 { value.value }\n",
    );
    repo.write(
        "src/included.rs",
        "use crate::origin::Payload as Data; include!(\"included_body.rs\");\n",
    );
    repo.write(
        "src/included_body.rs",
        "pub fn read(value: Data) -> u8 { value.value }\n",
    );
    repo.write(
        "src/unknown.rs",
        "use external::Payload as Data; pub fn read(value: Data) -> u8 { value.value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");
    repo.write(
        "src/walk.rs",
        "pub fn collect(value: u64) -> u64 { value }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    for (source, caller) in [
        ("src/origin.rs", "src/type_user.rs"),
        ("src/origin.rs", "src/public_bridge.rs"),
        ("src/origin.rs", "src/parent/child.rs"),
        ("src/origin.rs", "src/private_parent.rs"),
        ("src/origin.rs", "src/nested_parent.rs"),
        ("src/origin.rs", "src/included.rs"),
        ("src/origin.rs", "src/unknown.rs"),
        ("src/walk.rs", "src/function_user.rs"),
    ] {
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == source)
            .unwrap();
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers.iter().any(|entry| entry["path"] == caller)),
            "{source} -> {caller}: {context}"
        );
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_none_or(|callers| callers.iter().all(|entry| entry["path"] != "src/unused.rs")),
            "unused alias: {context}"
        );
    }
}

#[test]
fn impact_rust_path_dependency_alias_remains_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.create_dir_all("shapes/src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshapes = { path = \"shapes\" }\n",
    );
    repo.write(
        "shapes/Cargo.toml",
        "[package]\nname = \"shapes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "use shapes::Range as ShapeRange; pub fn end(value: ShapeRange) -> u8 { value.end }\n",
    );
    repo.write("shapes/src/lib.rs", "pub struct Range { pub end: u8 }\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write("shapes/src/lib.rs", "pub struct Range { pub end: u64 }\n");

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "shapes/src/lib.rs")
        .unwrap();
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|entry| entry["path"] == "src/lib.rs")),
        "path dependency alias: {context}"
    );
}

#[test]
fn impact_rust_external_aliases_do_not_become_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nanyhow = \"1\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin; pub mod external_std; pub mod external_dep; pub mod local; pub mod uncertain; pub mod decorated;\n",
    );
    repo.write(
        "src/origin.rs",
        "pub struct Error { pub value: u8 }\npub struct Result { pub value: u8 }\n",
    );
    repo.write(
        "src/external_std.rs",
        "use std::io::Error as IoError; pub fn read(_: IoError) {}\n",
    );
    repo.write(
        "src/external_dep.rs",
        "use anyhow::Result as AnyResult; pub fn read(_: AnyResult<()>) {}\n",
    );
    repo.write(
        "src/local.rs",
        "use crate::origin::Result as LocalResult; pub fn read(_: LocalResult) {}\n",
    );
    repo.write(
        "src/uncertain.rs",
        "use unknown::Result as MaybeResult; pub fn read(_: MaybeResult) {}\n",
    );
    repo.write(
        "src/decorated.rs",
        "#[serde(rename_all = \"camelCase\")] pub struct Marker;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/origin.rs",
        "pub struct Error { pub value: u64 }\npub struct Result { pub value: u64 }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/origin.rs")
        .unwrap();
    for file in ["src/external_std.rs", "src/external_dep.rs"] {
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_none_or(|callers| callers.iter().all(|entry| entry["path"] != file)),
            "external alias {file}: {context}"
        );
    }
    for file in ["src/local.rs", "src/uncertain.rs"] {
        assert!(
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers.iter().any(|entry| entry["path"] == file)),
            "local or uncertain alias {file}: {context}"
        );
    }
}

/// registry 依存の別名の証明は、Cargo が読む設定 (パッケージの祖先の `.cargo/config(.toml)` と
/// `$CARGO_HOME/config(.toml)`) を、リポジトリがホームの下にあるかどうかに関係なく同じ規則で
/// 調べる。依存の取得元やコンパイラを変える設定・環境変数や、解析できない設定があるときだけ、
/// 別名の import を呼び出し元に残す。
#[test]
fn impact_rust_external_alias_reads_cargo_configs_wherever_the_repository_lives() {
    const NEUTRAL: &str = "[build]\njobs = 2\n[net]\ngit-fetch-with-cli = true\n";
    const PATCH: &str = "[patch.crates-io]\nanyhow = { path = \"../anyhow\" }\n";
    const SOURCE: &str = "[source.crates-io]\nreplace-with = \"vendored\"\n[source.vendored]\ndirectory = \"vendor\"\n";
    // 一時ディレクトリを利用者のファイルシステムに見立てる。`$CARGO_HOME` は `home/.cargo`。
    // `home/GitHub/repo` はホームの下のリポジトリで、ホームの設定を祖先の設定としても読む。
    // `work/repo` はホームの外。
    // (`$CARGO_HOME/config.toml`, 環境変数, リポジトリ,
    //  [(パッケージ, リポジトリに置く設定, 別名の import が残るか)])
    let runs = [
        (
            NEUTRAL,
            &[][..],
            "home/GitHub/repo",
            vec![("sample", None, false)],
        ),
        (
            NEUTRAL,
            &[][..],
            "work/repo",
            vec![
                ("sample", None, false),
                (
                    "plain/sample",
                    Some(("plain/.cargo/config", NEUTRAL)),
                    false,
                ),
                (
                    "vendored/sample",
                    Some(("vendored/.cargo/config.toml", SOURCE)),
                    true,
                ),
                (
                    "broken/sample",
                    Some(("broken/sample/.cargo/config.toml", "[patch\n")),
                    true,
                ),
                (
                    "loader/sample",
                    Some((
                        "loader/.cargo/config.toml",
                        "[env]\nLD_PRELOAD = { value = \"custom.so\", force = true }\n",
                    )),
                    true,
                ),
                (
                    "path/sample",
                    Some((
                        "path/.cargo/config.toml",
                        "[env]\nPATH = { value = \"tools\", force = true }\n",
                    )),
                    true,
                ),
            ],
        ),
        (
            PATCH,
            &[][..],
            "home/GitHub/repo",
            vec![("sample", None, true)],
        ),
        (PATCH, &[][..], "work/repo", vec![("sample", None, true)]),
        (
            NEUTRAL,
            &[("RUSTC_WRAPPER", "sccache")][..],
            "work/repo",
            vec![("sample", None, true)],
        ),
        (
            NEUTRAL,
            &[(
                "CARGO_ENCODED_RUSTFLAGS",
                "-C\u{1f}opt-level=1\u{1f}@flags.txt",
            )][..],
            "work/repo",
            vec![("sample", None, true)],
        ),
    ];
    let mut failures = Vec::new();
    for (home_config, environment, repository, packages) in runs {
        let root = TestRepo::new();
        root.create_dir_all("home/.cargo");
        root.write("home/.cargo/config.toml", home_config);
        for (package, config, _) in &packages {
            let package = format!("{repository}/{package}");
            root.create_dir_all(format!("{package}/src"));
            root.write(
                format!("{package}/Cargo.toml"),
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nanyhow = \"1\"\n",
            );
            root.write(
                format!("{package}/src/lib.rs"),
                "pub mod origin; pub mod external_dep; pub mod local;\n",
            );
            root.write(
                format!("{package}/src/origin.rs"),
                "pub struct Result { pub value: u8 }\n",
            );
            root.write(
                format!("{package}/src/external_dep.rs"),
                "use anyhow::Result as AnyResult; pub fn read(_: AnyResult<()>) {}\n",
            );
            root.write(
                format!("{package}/src/local.rs"),
                "use crate::origin::Result as LocalResult; pub fn read(_: LocalResult) {}\n",
            );
            if let Some((path, content)) = config {
                let path = format!("{repository}/{path}");
                root.create_dir_all(std::path::Path::new(&path).parent().unwrap());
                root.write(path, content);
            }
        }
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "astro-sight@example.com"],
            &["config", "user.name", "astro-sight"],
            &["add", "."],
            &["commit", "-m", "initial", "-q"],
        ] {
            root.git(["-C", repository].iter().chain(args));
        }
        for (package, _, _) in &packages {
            root.write(
                format!("{repository}/{package}/src/origin.rs"),
                "pub struct Result { pub value: u64 }\n",
            );
        }

        let cargo_home = root.path("home/.cargo");
        let envs = environment
            .iter()
            .map(|(name, value)| (*name, std::ffi::OsStr::new(value)))
            .chain([("CARGO_HOME", cargo_home.as_os_str())]);
        let context = root.run_json_in(repository, envs, "context", &["--git"]);
        for (package, config, keeps_alias) in &packages {
            let callers = context["changes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|change| change["path"] == format!("{package}/src/origin.rs"))
                .and_then(|change| change["impacted_callers"].as_array())
                .map(|callers| {
                    callers
                        .iter()
                        .filter_map(|entry| entry["path"].as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !callers.contains(&format!("{package}/src/local.rs").as_str())
                || callers.contains(&format!("{package}/src/external_dep.rs").as_str())
                    != *keeps_alias
            {
                failures.push(format!(
                    "home {home_config:?}, env {environment:?}, {repository}/{package} {config:?}: {callers:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// review --staged は index を一時ディレクトリに書き出して解析する。そこからは Cargo が実際に
/// 読む設定 (元のリポジトリの祖先と、追跡していない `.cargo/config.toml`) が見えないので、
/// registry 依存の別名を証明せず呼び出し元に残す。作業ツリーの review では従来どおり外す。
#[test]
fn impact_rust_external_alias_stays_in_staged_review() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nanyhow = \"1\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin; pub mod external_dep; pub mod local;\n",
    );
    repo.write("src/origin.rs", "pub struct Result { pub value: u8 }\n");
    repo.write(
        "src/external_dep.rs",
        "use anyhow::Result as AnyResult; pub fn read(_: AnyResult<()>) {}\n",
    );
    repo.write(
        "src/local.rs",
        "use crate::origin::Result as LocalResult; pub fn read(_: LocalResult) {}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/origin.rs", "pub struct Result { pub value: u64 }\n");
    repo.stage_all();

    for (args, keeps_alias) in [(&["--git"][..], false), (&["--git", "--staged"][..], true)] {
        let review = repo.run_json("review", args);
        let callers = review["impact"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/origin.rs")
            .and_then(|change| change["impacted_callers"].as_array())
            .map(|callers| {
                callers
                    .iter()
                    .filter_map(|entry| entry["path"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(callers.contains(&"src/local.rs"), "{args:?}: {review}");
        assert_eq!(
            callers.contains(&"src/external_dep.rs"),
            keeps_alias,
            "{args:?}: {review}"
        );
    }
}

#[test]
fn impact_rust_reexport_owner_survives_unrelated_module_wiring() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin; pub mod other; pub mod direct; pub mod unrelated; pub mod ambiguous;\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write("src/other.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write(
        "src/direct.rs",
        "pub fn read(value: crate::origin::Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/unrelated.rs",
        "pub fn read(value: crate::other::Payload) -> u8 { value.value }\n",
    );
    repo.write(
        "src/ambiguous.rs",
        "use crate::origin::*; use crate::other::*; pub fn read(value: Payload) -> u8 { value.value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");
    repo.write(
        "src/lib.rs",
        "pub mod origin; pub mod other; pub mod direct; pub mod unrelated; pub mod ambiguous; pub mod newf;\n",
    );
    repo.write("src/newf.rs", "use crate::origin::Payload;\n");

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/origin.rs")
        .unwrap();
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|entry| entry["path"] == "src/direct.rs")),
        "direct caller: {context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(|callers| callers
                .iter()
                .all(|entry| entry["path"] != "src/unrelated.rs")),
            "unrelated caller in {bucket}: {context}"
        );
    }
    assert!(
        [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers"
        ]
        .iter()
        .any(
            |bucket| change[bucket].as_array().is_some_and(|callers| callers
                .iter()
                .any(|entry| entry["path"] == "src/ambiguous.rs"))
        ),
        "ambiguous glob must remain visible: {context}"
    );
}

#[test]
fn impact_rust_parent_glob_follows_the_imported_function() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/true_parent");
    repo.create_dir_all("src/other_parent");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod walk; pub mod other; pub mod bridge; pub mod other_bridge; pub mod bridge_user; pub mod other_bridge_user; pub mod true_parent; pub mod other_parent;\n",
    );
    repo.write("src/walk.rs", "pub fn collect(value: u8) -> u8 { value }\n");
    repo.write(
        "src/other.rs",
        "pub fn collect(value: u8) -> u8 { value }\n",
    );
    repo.write("src/bridge.rs", "pub use crate::walk::collect;\n");
    repo.write("src/other_bridge.rs", "pub use crate::other::collect;\n");
    repo.write(
        "src/bridge_user.rs",
        "pub fn call() -> u8 { crate::bridge::collect(1) }\n",
    );
    repo.write(
        "src/other_bridge_user.rs",
        "pub fn call() -> u8 { crate::other_bridge::collect(1) }\n",
    );
    repo.write(
        "src/true_parent.rs",
        "use crate::walk::collect; pub mod child;\n",
    );
    repo.write(
        "src/true_parent/child.rs",
        "use super::*; pub fn call() -> u8 { collect(1) }\n",
    );
    repo.write(
        "src/other_parent.rs",
        "use crate::other::collect; pub mod child;\n",
    );
    repo.write(
        "src/other_parent/child.rs",
        "use super::*; pub fn call() -> u8 { collect(1) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/walk.rs",
        "pub fn collect(value: u64) -> u64 { value }\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/walk.rs")
        .unwrap();
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|entry| entry["path"] == "src/true_parent/child.rs")),
        "true caller: {context}"
    );
    assert!(
        change["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers
                .iter()
                .any(|entry| entry["path"] == "src/bridge_user.rs")),
        "true bridge caller: {context}"
    );
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket].as_array().is_none_or(|callers| callers
                .iter()
                .all(|entry| entry["path"] != "src/other_parent/child.rs"
                    && entry["path"] != "src/other_bridge_user.rs")),
            "unrelated caller in {bucket}: {context}"
        );
    }
}

#[test]
fn impact_rust_reexport_retarget_keeps_old_side_caller() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod origin; pub mod other; pub mod bridge; pub mod caller;\n",
    );
    repo.write("src/origin.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write("src/other.rs", "pub struct Payload { pub value: u8 }\n");
    repo.write("src/bridge.rs", "pub use crate::origin::Payload;\n");
    repo.write(
        "src/caller.rs",
        "pub fn read(value: crate::bridge::Payload) -> u8 { value.value }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/origin.rs", "pub struct Payload { pub value: u64 }\n");
    repo.write("src/bridge.rs", "pub use crate::other::Payload;\n");

    let context = repo.run_json("context", &["--git"]);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/origin.rs")
        .unwrap();
    assert!(
        [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers"
        ]
        .iter()
        .any(|bucket| change[bucket]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|entry| entry["path"] == "src/caller.rs"))),
        "old binding must remain visible: {context}"
    );
}
