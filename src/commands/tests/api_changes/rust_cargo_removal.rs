//! Rust の削除した自由関数と、Cargo の依存で削除元の package に届かない同名の参照 (Issue #111)。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

const BOTH_MEMBERS: &str = "[workspace]\nmembers = [\"old_app\", \"engine\"]\nresolver = \"2\"\n";
const ENGINE_ONLY: &str = "[workspace]\nmembers = [\"engine\"]\nresolver = \"2\"\n";
const OLD_RUN: &str = "pub fn run() -> u32 {\n    7\n}\n";
const ENGINE_RUN: &str =
    "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    run()\n}\n";
// #110 が追わない use 経由の参照を残し、Cargo の証明を使う対照にする。
const ENGINE_IMPORT_RUN: &str = "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    run()\n}\n\nmod indirect {\n    use super::run;\n    pub fn call() -> u32 {\n        run()\n    }\n}\n";

fn package(name: &str, extra: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}")
}

/// `base` をコミットし、`removed` のディレクトリを消して `changed` を書いた作業ツリーの API 差分。
fn api_after(base: &[(&str, &str)], removed: &[&str], changed: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, base, "base");
    for path in removed {
        fs::remove_dir_all(repo.join(path)).unwrap();
    }
    for (path, source) in changed {
        let full = repo.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, source).unwrap();
    }
    detect_api_changes_from_worktree(repo)
}

/// old_app をまるごと消し、ルートの members から外す。`engine` は変更前後の engine/Cargo.toml。
fn remove_old_app(engine: (&str, &str), old_source: &str, engine_source: &str) -> ApiChanges {
    let (old_manifest, new_manifest) = (package("engine", engine.0), package("engine", engine.1));
    let mut changed = vec![("Cargo.toml", ENGINE_ONLY)];
    if new_manifest != old_manifest {
        changed.push(("engine/Cargo.toml", &new_manifest));
    }
    api_after(
        &[
            ("Cargo.toml", BOTH_MEMBERS),
            ("old_app/Cargo.toml", &package("old_app", "")),
            ("old_app/src/lib.rs", old_source),
            ("engine/Cargo.toml", &old_manifest),
            ("engine/src/lib.rs", engine_source),
        ],
        &["old_app"],
        &changed,
    )
}

fn is_dead(api: &ApiChanges, name: &str) -> bool {
    assert!(
        api.removed.iter().any(|s| s.name == name)
            != api.removed_dead.iter().any(|s| s.name == name),
        "{name} は removed と removed_dead のどちらか一方に入る: {api:?}"
    );
    api.removed_dead.iter().any(|s| s.name == name)
}

fn remove_injected_run(repo: &std::path::Path) {
    fs::create_dir_all(repo).unwrap();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", BOTH_MEMBERS),
            ("old_app/Cargo.toml", &package("alpha", "")),
            ("old_app/src/lib.rs", OLD_RUN),
            ("engine/Cargo.toml", &package("beta", "")),
            (
                "engine/src/lib.rs",
                "pub fn drive() -> u32 {\n    injected::run()\n}\n",
            ),
        ],
        "base",
    );
    fs::write(repo.join("old_app/src/lib.rs"), "").unwrap();
}

#[test]
fn rust_cargo_removal_unreachable_package_does_not_count_same_named_function() {
    for source in [ENGINE_RUN, ENGINE_IMPORT_RUN] {
        let api = remove_old_app(("", ""), OLD_RUN, source);
        assert!(is_dead(&api, "run"), "{source}: {api:?}");
    }
    // engine に同名の関数が無い場合は従来どおり removed_dead。
    let api = remove_old_app(("", ""), OLD_RUN, "pub fn drive() -> u32 {\n    1\n}\n");
    assert!(is_dead(&api, "run"), "{api:?}");
}

#[test]
fn rust_cargo_removal_ancestor_extern_configuration_keeps_api_rm() {
    for (config, dead) in [
        ("[build]\njobs = 1\n", true),
        (
            "[build]\nrustflags = [\"--extern\", \"injected=../libalpha.rlib\"]\n",
            false,
        ),
    ] {
        let outer = tempfile::tempdir().unwrap();
        let repo = outer.path().join("workspace");
        fs::create_dir_all(outer.path().join(".cargo")).unwrap();
        fs::write(outer.path().join(".cargo/config.toml"), config).unwrap();
        remove_injected_run(&repo);
        let api = detect_api_changes_from_worktree(&repo);
        assert_eq!(is_dead(&api, "run"), dead, "{config}: {api:?}");
    }
}

#[test]
#[cfg(any(unix, windows))]
fn rust_cargo_removal_checks_both_ancestors_through_directory_alias() {
    let outer = tempfile::tempdir().unwrap();
    let physical = outer.path().join("physical/nested");
    let repo = physical.join("workspace");
    remove_injected_run(&repo);
    let logical = outer.path().join("logical");
    fs::create_dir_all(&logical).unwrap();
    let alias = logical.join("alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&physical, &alias).unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "New-Item -ItemType Junction -Path $env:ASTRO_SIGHT_TEST_JUNCTION_PATH -Target ([System.Management.Automation.WildcardPattern]::Escape($env:ASTRO_SIGHT_TEST_JUNCTION_TARGET)) -ErrorAction Stop | Out-Null",
            ])
            .env("ASTRO_SIGHT_TEST_JUNCTION_PATH", &alias)
            .env("ASTRO_SIGHT_TEST_JUNCTION_TARGET", &physical)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let aliased_repo = alias.join("workspace");
    assert!(is_dead(
        &detect_api_changes_from_worktree(&aliased_repo),
        "run"
    ));
    // どちらの経路にだけ設定があっても証明しない。
    for ancestor in [physical.parent().unwrap(), logical.as_path()] {
        fs::create_dir_all(ancestor.join(".cargo")).unwrap();
        let config = ancestor.join(".cargo/config.toml");
        fs::write(&config, "[build]\nrustc-wrapper = 'other-rustc'\n").unwrap();
        assert!(!is_dead(
            &detect_api_changes_from_worktree(&aliased_repo),
            "run"
        ));
        fs::remove_file(config).unwrap();
    }
    assert!(is_dead(
        &detect_api_changes_from_worktree(&aliased_repo),
        "run"
    ));
}

#[test]
fn rust_cargo_removal_dependency_on_removed_package_keeps_api_rm() {
    for dependency in [
        "[dependencies]\nold_app = { path = \"../old_app\" }\n",
        "[dev-dependencies]\nold_app = { path = \"../old_app\" }\n",
        "[build-dependencies]\nold_app = { path = \"../old_app\" }\n",
        "[target.'cfg(unix)'.dependencies]\nold_app = { path = \"../old_app\" }\n",
        "[dependencies]\nold_app = { path = \"../old_app\", optional = true }\n",
        "[dependencies]\nlegacy = { package = \"old_app\", path = \"../old_app\" }\n",
    ] {
        // 変更後も依存が残る (消したディレクトリを指す) 場合と、依存も同時に消した場合。
        for new_dependency in [dependency, ""] {
            let api = remove_old_app((dependency, new_dependency), OLD_RUN, ENGINE_IMPORT_RUN);
            assert!(
                !is_dead(&api, "run"),
                "{dependency} -> {new_dependency:?}: {api:?}"
            );
        }
    }
}

#[test]
fn rust_cargo_removal_workspace_inherited_dependency_keeps_api_rm() {
    let root = "[workspace]\nmembers = [\"old_app\", \"engine\"]\nresolver = \"2\"\n[workspace.dependencies]\nold_app = { path = \"old_app\" }\n";
    let api = api_after(
        &[
            ("Cargo.toml", root),
            ("old_app/Cargo.toml", &package("old_app", "")),
            ("old_app/src/lib.rs", OLD_RUN),
            (
                "engine/Cargo.toml",
                &package("engine", "[dependencies]\nold_app.workspace = true\n"),
            ),
            ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
        ],
        &["old_app"],
        &[
            ("Cargo.toml", ENGINE_ONLY),
            ("engine/Cargo.toml", &package("engine", "")),
        ],
    );
    assert!(!is_dead(&api, "run"), "{api:?}");
}

#[test]
fn rust_cargo_removal_transitive_dependency_keeps_api_rm() {
    let base_mid = package(
        "mid",
        "[dependencies]\nold_app = { path = \"../old_app\" }\n",
    );
    let engine = package("engine", "[dependencies]\nmid = { path = \"../mid\" }\n");
    // 変更後の mid が消したディレクトリを指し続ける場合と、mid の依存も消した場合。
    for new_mid in [base_mid.clone(), package("mid", "")] {
        let api = api_after(
            &[
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"old_app\", \"mid\", \"engine\"]\nresolver = \"2\"\n",
                ),
                ("old_app/Cargo.toml", &package("old_app", "")),
                ("old_app/src/lib.rs", OLD_RUN),
                ("mid/Cargo.toml", &base_mid),
                ("mid/src/lib.rs", "pub fn middle() -> u32 {\n    2\n}\n"),
                ("engine/Cargo.toml", &engine),
                ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
            ],
            &["old_app"],
            &[
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"mid\", \"engine\"]\nresolver = \"2\"\n",
                ),
                ("mid/Cargo.toml", &new_mid),
            ],
        );
        assert!(!is_dead(&api, "run"), "{new_mid}: {api:?}");
    }
}

#[test]
fn rust_cargo_removal_dependency_added_after_the_change_keeps_api_rm() {
    // old_app は残し、run だけを消す。engine は変更後に old_app へ依存する。
    let api = api_after(
        &[
            ("Cargo.toml", BOTH_MEMBERS),
            ("old_app/Cargo.toml", &package("old_app", "")),
            ("old_app/src/lib.rs", OLD_RUN),
            ("engine/Cargo.toml", &package("engine", "")),
            ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
        ],
        &[],
        &[
            ("old_app/src/lib.rs", "pub fn launch() -> u32 {\n    7\n}\n"),
            (
                "engine/Cargo.toml",
                &package(
                    "engine",
                    "[dependencies]\nold_app = { path = \"../old_app\" }\n",
                ),
            ),
        ],
    );
    assert!(!is_dead(&api, "run"), "{api:?}");
    // 依存を足さなければ、残る old_app にも届かない。
    let api = api_after(
        &[
            ("Cargo.toml", BOTH_MEMBERS),
            ("old_app/Cargo.toml", &package("old_app", "")),
            ("old_app/src/lib.rs", OLD_RUN),
            ("engine/Cargo.toml", &package("engine", "")),
            ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
        ],
        &[],
        &[("old_app/src/lib.rs", "pub fn launch() -> u32 {\n    7\n}\n")],
    );
    assert!(is_dead(&api, "run"), "{api:?}");
}

#[test]
fn rust_cargo_removal_foreign_or_attributed_function_keeps_api_rm() {
    for old_source in [
        "#[no_mangle]\npub extern \"C\" fn run() -> u32 {\n    7\n}\n",
        "#[unsafe(no_mangle)]\npub extern \"C\" fn run() -> u32 {\n    7\n}\n",
        "pub extern \"C\" fn run() -> u32 {\n    7\n}\n",
        "pub extern fn run() -> u32 {\n    7\n}\n",
        "pub unsafe extern \"C\" fn run() -> u32 {\n    7\n}\n",
        "#[export_name = \"run\"]\npub fn run() -> u32 {\n    7\n}\n",
        "#[old_macros::exported]\npub fn run() -> u32 {\n    7\n}\n",
        "#[cfg_attr(all(), no_mangle)]\npub fn run() -> u32 {\n    7\n}\n",
    ] {
        let api = remove_old_app(("", ""), old_source, ENGINE_IMPORT_RUN);
        assert!(!is_dead(&api, "run"), "{old_source}: {api:?}");
    }
}

#[test]
fn rust_cargo_removal_same_package_survivor_is_unchanged() {
    // 直接の呼び出しは #110 が独立定義へ解決する。未証明の import は同じ package なので残す。
    for (source, dead) in [(ENGINE_RUN, true), (ENGINE_IMPORT_RUN, false)] {
        let api = api_after(
            &[
                ("Cargo.toml", BOTH_MEMBERS),
                ("old_app/Cargo.toml", &package("old_app", "")),
                ("old_app/src/lib.rs", "pub mod a;\npub mod b;\n"),
                ("old_app/src/a.rs", OLD_RUN),
                ("old_app/src/b.rs", source),
                ("engine/Cargo.toml", &package("engine", "")),
                ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
            ],
            &[],
            &[("old_app/src/a.rs", "pub fn launch() -> u32 {\n    7\n}\n")],
        );
        assert_eq!(is_dead(&api, "run"), dead, "{source}: {api:?}");
    }
}

#[test]
fn rust_cargo_removal_caller_moved_out_of_removed_package_keeps_api_rm() {
    // 移動先の直接パスは #110 で解決する。import 経由なら旧 package の到達性も必要になる。
    for (caller, dead) in [
        (
            "pub fn call_run() -> u32 {\n    super::run() + 40\n}\n",
            true,
        ),
        (
            "use super::run;\npub fn call_run() -> u32 {\n    run() + 40\n}\n",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", BOTH_MEMBERS),
                ("old_app/Cargo.toml", &package("old_app", "")),
                ("old_app/src/lib.rs", &format!("pub mod caller;\n{OLD_RUN}")),
                ("old_app/src/caller.rs", caller),
                ("engine/Cargo.toml", &package("engine", "")),
                ("engine/src/lib.rs", ENGINE_RUN),
            ],
            "base",
        );
        git_in(
            repo,
            &["mv", "old_app/src/caller.rs", "engine/src/caller.rs"],
        );
        git_in(repo, &["rm", "-r", "-q", "old_app"]);
        fs::write(repo.join("Cargo.toml"), ENGINE_ONLY).unwrap();
        fs::write(
            repo.join("engine/src/lib.rs"),
            format!("pub mod caller;\n{ENGINE_RUN}"),
        )
        .unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert_eq!(is_dead(&api, "run"), dead, "{caller}: {api:?}");
    }
}

#[test]
fn rust_cargo_removal_new_crate_needs_a_binding_in_the_same_file() {
    // 旧クレートを消し、同じ差分で作った新しいクレートが同名の関数を呼ぶ。
    // 署名が同じなら移動 (moved) になるので、戻り値の型を変えて削除として扱わせる。
    let base = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"old_app\"]\nresolver = \"2\"\n",
        ),
        ("old_app/Cargo.toml", &package("old_app", "")),
        ("old_app/src/lib.rs", OLD_RUN),
    ];
    let new_app = package("new_app", "");
    let own_run = "pub fn run() -> u64 {\n    1\n}\n";
    let variants: [(&[(&str, String)], bool); 7] = [
        // 同じファイル直下の関数を修飾なしで呼ぶ。
        (
            &[(
                "new_app/src/lib.rs",
                format!("{own_run}\npub fn drive() -> u64 {{\n    run()\n}}\n"),
            )],
            true,
        ),
        // 依存を書き忘れた古い呼び出しと、束縛先の無い呼び出しは残す。
        (
            &[(
                "new_app/src/lib.rs",
                format!("{own_run}\npub fn drive() -> u32 {{\n    old_app::run()\n}}\n"),
            )],
            false,
        ),
        (
            &[(
                "new_app/src/lib.rs",
                "pub fn drive() -> u32 {\n    run()\n}\n".to_owned(),
            )],
            false,
        ),
        // library と別の crate root (bin) からの `crate::run` と、inline の mod の中からの
        // 呼び出しは、同じ package の関数に解決するとは限らない。
        (
            &[
                ("new_app/src/lib.rs", own_run.to_owned()),
                (
                    "new_app/src/main.rs",
                    "fn main() {\n    crate::run();\n}\n".to_owned(),
                ),
            ],
            false,
        ),
        (
            &[(
                "new_app/src/lib.rs",
                format!(
                    "{own_run}\npub mod inner {{\n    pub fn drive() -> u64 {{\n        run()\n    }}\n}}\n"
                ),
            )],
            false,
        ),
        // 既知の属性は #110 が証明し、未知の属性が付いた item は保留する。
        (
            &[(
                "new_app/src/lib.rs",
                format!("{own_run}\n#[inline]\npub fn drive() -> u64 {{\n    run()\n}}\n"),
            )],
            true,
        ),
        (
            &[(
                "new_app/src/lib.rs",
                format!(
                    "{own_run}\n#[attributes::rewrite]\npub fn drive() -> u64 {{\n    run()\n}}\n"
                ),
            )],
            false,
        ),
    ];
    for (files, dead) in variants {
        let mut changed = vec![
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"new_app\"]\nresolver = \"2\"\n",
            ),
            ("new_app/Cargo.toml", new_app.as_str()),
        ];
        changed.extend(files.iter().map(|(path, source)| (*path, source.as_str())));
        let api = api_after(&base, &["old_app"], &changed);
        assert_eq!(is_dead(&api, "run"), dead, "{files:?}: {api:?}");
    }
}

#[test]
fn rust_cargo_removal_call_written_in_this_diff_needs_a_binding() {
    let unbound = "pub fn drive() -> u32 {\n    run()\n}\n";
    // 既存の engine のファイルを、削除と同じ差分で書き換える。
    for (old_engine, new_engine, attributes, dead) in [
        // 束縛先の無い呼び出しを書き足した (消した関数の呼び出しを写した) 場合は残す。
        ("pub fn drive() -> u32 {\n    1\n}\n", unbound, "", false),
        // `-diff` の属性で hunk が出ないファイルも、中身を比べて書き換えたと分かる。
        (
            "pub fn drive() -> u32 {\n    1\n}\n",
            unbound,
            "engine/src/lib.rs -diff\n",
            false,
        ),
        // 同じファイル直下の関数を修飾なしで呼ぶなら、書き換えたファイルでも外す。
        (
            "pub fn drive() -> u32 {\n    1\n}\n",
            "pub fn run() -> u64 {\n    1\n}\n\npub fn drive() -> u64 {\n    run()\n}\n",
            "",
            true,
        ),
        (
            ENGINE_RUN,
            "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    run()\n}\n\npub fn extra() -> u32 {\n    2\n}\n",
            "",
            true,
        ),
    ] {
        let api = api_after(
            &[
                ("Cargo.toml", BOTH_MEMBERS),
                (".gitattributes", attributes),
                ("old_app/Cargo.toml", &package("old_app", "")),
                ("old_app/src/lib.rs", OLD_RUN),
                ("engine/Cargo.toml", &package("engine", "")),
                ("engine/src/lib.rs", old_engine),
            ],
            &["old_app"],
            &[
                ("Cargo.toml", ENGINE_ONLY),
                ("engine/src/lib.rs", new_engine),
            ],
        );
        assert_eq!(
            is_dead(&api, "run"),
            dead,
            "{attributes}{new_engine}: {api:?}"
        );
    }
}

#[test]
fn rust_cargo_removal_mention_of_removed_crate_keeps_api_rm() {
    // engine の別のファイルが消したクレートを名前で使っている (glob の再公開)。
    let api = api_after(
        &[
            ("Cargo.toml", BOTH_MEMBERS),
            ("old_app/Cargo.toml", &package("old_app", "")),
            ("old_app/src/lib.rs", OLD_RUN),
            ("engine/Cargo.toml", &package("engine", "")),
            (
                "engine/src/lib.rs",
                &format!("pub mod shim;\n{ENGINE_IMPORT_RUN}"),
            ),
            ("engine/src/shim.rs", "pub use old_app::*;\n"),
        ],
        &["old_app"],
        &[("Cargo.toml", ENGINE_ONLY)],
    );
    assert!(!is_dead(&api, "run"), "{api:?}");
}

#[test]
fn rust_cargo_removal_macro_in_either_tree_keeps_api_rm() {
    // マクロはソースを取り込めるので、ワークスペースのどこかにあれば証明しない。
    for (old_source, engine_source) in [
        (
            OLD_RUN,
            "pub fn run() -> u32 {\n    1\n}\n\npub fn drive() -> u32 {\n    assert!(run() > 0);\n    run()\n}\n",
        ),
        (
            "pub fn run() -> u32 {\n    vec![7].len() as u32\n}\n",
            ENGINE_IMPORT_RUN,
        ),
    ] {
        let api = remove_old_app(("", ""), old_source, engine_source);
        assert!(
            !is_dead(&api, "run"),
            "{old_source}{engine_source}: {api:?}"
        );
    }
}

#[test]
fn rust_cargo_removal_same_named_external_dependency_keeps_api_rm() {
    for dependency in [
        "[dependencies]\nold_app = \"0.1\"\n",
        "[dependencies]\nlegacy = { package = \"old-app\", git = \"https://example.com/old_app\" }\n",
    ] {
        let api = remove_old_app(("", dependency), OLD_RUN, ENGINE_IMPORT_RUN);
        assert!(!is_dead(&api, "run"), "{dependency}: {api:?}");
    }
    // 名前の違う外部依存は妨げない。
    let api = remove_old_app(
        ("", "[dependencies]\nserde = \"1\"\n"),
        OLD_RUN,
        ENGINE_IMPORT_RUN,
    );
    assert!(is_dead(&api, "run"), "{api:?}");
}

#[test]
fn rust_cargo_removal_keeps_api_rm_when_another_caller_could_name_the_package() {
    // engine の呼び出しは外せても、変更前に old_app へ依存していた mid の呼び出しは外せない。
    let api = api_after(
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"old_app\", \"mid\", \"engine\"]\nresolver = \"2\"\n",
            ),
            ("old_app/Cargo.toml", &package("old_app", "")),
            ("old_app/src/lib.rs", OLD_RUN),
            (
                "mid/Cargo.toml",
                &package(
                    "mid",
                    "[dependencies]\nold_app = { path = \"../old_app\" }\n",
                ),
            ),
            ("mid/src/lib.rs", ENGINE_IMPORT_RUN),
            ("engine/Cargo.toml", &package("engine", "")),
            ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
        ],
        &["old_app"],
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"mid\", \"engine\"]\nresolver = \"2\"\n",
            ),
            ("mid/Cargo.toml", &package("mid", "")),
        ],
    );
    assert!(!is_dead(&api, "run"), "{api:?}");
}

#[test]
fn rust_cargo_removal_moved_or_renamed_package_is_still_the_removed_package() {
    let old_lib = "pub fn run() -> u32 {\n    7\n}\n\npub fn keep() -> u32 {\n    3\n}\n";
    // 別のディレクトリと名前へ移す場合と、同じディレクトリで名前だけを変える場合。
    for (directory, name) in [("legacy", "legacy"), ("old_app", "renamed")] {
        for depends in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            init_git_repo_for_test(repo);
            git_commit_files(
                repo,
                &[
                    ("Cargo.toml", BOTH_MEMBERS),
                    ("old_app/Cargo.toml", &package("old_app", "")),
                    ("old_app/src/lib.rs", old_lib),
                    ("engine/Cargo.toml", &package("engine", "")),
                    ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
                ],
                "base",
            );
            if directory != "old_app" {
                git_in(repo, &["mv", "old_app", directory]);
            }
            fs::write(
                repo.join(format!("{directory}/Cargo.toml")),
                package(name, ""),
            )
            .unwrap();
            fs::write(
                repo.join(format!("{directory}/src/lib.rs")),
                "pub fn keep() -> u32 {\n    3\n}\n",
            )
            .unwrap();
            fs::write(
                repo.join("Cargo.toml"),
                format!("[workspace]\nmembers = [\"{directory}\", \"engine\"]\nresolver = \"2\"\n"),
            )
            .unwrap();
            if depends {
                fs::write(
                    repo.join("engine/Cargo.toml"),
                    package(
                        "engine",
                        &format!("[dependencies]\n{name} = {{ path = \"../{directory}\" }}\n"),
                    ),
                )
                .unwrap();
            }
            let api = detect_api_changes_from_worktree(repo);
            assert_eq!(
                is_dead(&api, "run"),
                !depends,
                "{directory} {name} {depends}: {api:?}"
            );
        }
    }
}

/// 変更前のツリーに git の要素を 1 つ足してコミットし、作業ツリーにも書き出す。
fn commit_tree_entry(repo: &std::path::Path, mode: &str, path: &str, content: &str) {
    let git_output = |args: &[&str], input: Option<&str>| {
        let mut child = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
        }
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let object = if mode == "160000" {
        git_output(&["rev-parse", "HEAD"], None)
    } else {
        git_output(&["hash-object", "-w", "--stdin"], Some(content))
    };
    git_in(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{object},{path}"),
        ],
    );
    git_in(repo, &["commit", "-q", "-m", "entry"]);
    if mode == "160000" {
        fs::create_dir_all(repo.join(path)).unwrap();
    } else {
        git_in(repo, &["checkout", "HEAD", "--", path]);
    }
}

#[test]
fn rust_cargo_removal_unreadable_base_tree_keeps_api_rm() {
    let lfs = "version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 12\n";
    // ディレクトリへの symlink・gitlink・LFS のポインタは中身を確かめられない。ツリー内の
    // 通常ファイルへの symlink (文書の別名) は妨げない。
    for (mode, path, content, dead) in [
        ("120000", "docs", "old_app/src", false),
        ("160000", "third_party", "", false),
        ("100644", "engine/src/data.rs", lfs, false),
        ("120000", "AGENTS.md", "README.md", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", BOTH_MEMBERS),
                ("README.md", "readme\n"),
                ("old_app/Cargo.toml", &package("old_app", "")),
                ("old_app/src/lib.rs", OLD_RUN),
                ("engine/Cargo.toml", &package("engine", "")),
                ("engine/src/lib.rs", ENGINE_IMPORT_RUN),
            ],
            "base",
        );
        commit_tree_entry(repo, mode, path, content);
        fs::remove_dir_all(repo.join("old_app")).unwrap();
        fs::write(repo.join("Cargo.toml"), ENGINE_ONLY).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert_eq!(is_dead(&api, "run"), dead, "{mode} {path}: {api:?}");
    }
}

#[test]
fn rust_cargo_removal_lfs_pointer_in_worktree_keeps_api_rm() {
    let lfs = "version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 12\n";
    let crlf = lfs.replace('\n', "\r\n");
    let engine = format!("pub mod data;\n{ENGINE_IMPORT_RUN}");
    for (source, dead) in [
        ("pub const VALUE: u32 = 1;\n", true),
        (lfs, false),
        (crlf.as_str(), false),
    ] {
        let api = api_after(
            &[
                ("Cargo.toml", BOTH_MEMBERS),
                ("old_app/Cargo.toml", &package("old_app", "")),
                ("old_app/src/lib.rs", OLD_RUN),
                ("engine/Cargo.toml", &package("engine", "")),
                ("engine/src/lib.rs", &engine),
                ("engine/src/data.rs", "pub const VALUE: u32 = 1;\n"),
            ],
            &["old_app"],
            &[("Cargo.toml", ENGINE_ONLY), ("engine/src/data.rs", source)],
        );
        assert_eq!(is_dead(&api, "run"), dead, "{source}: {api:?}");
    }
}
