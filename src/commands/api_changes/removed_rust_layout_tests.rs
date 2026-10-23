use super::tests::{MANIFEST, site_in};
use super::*;

fn base() -> Vec<(&'static str, &'static str)> {
    vec![
        MANIFEST,
        ("src/lib.rs", "pub mod engine; pub mod app;"),
        ("src/engine.rs", "pub fn run() {}"),
        ("src/app.rs", "pub fn f() { super::engine::run(); }"),
    ]
}

#[test]
fn removed_rust_layout_closes_ignored_manifest_inputs() {
    for (manifest, extra) in [
        (
            "[workspace]\nmembers = [\".hidden\"]\n",
            vec![
                (".hidden/Cargo.toml", MANIFEST.1),
                (
                    ".hidden/src/lib.rs",
                    "#[path = \"../../src/app.rs\"] mod mirror;",
                ),
            ],
        ),
        (
            "[workspace]\nmembers = [\"crates/*\"]\n",
            vec![
                ("crates/a/Cargo.toml", MANIFEST.1),
                ("crates/a/src/lib.rs", "include!(\"../../../src/app.rs\");"),
            ],
        ),
        (
            "[dependencies]\na = { path = \".hidden\", optional = true }\n",
            vec![
                (
                    ".hidden/Cargo.toml",
                    "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nb = { path = \"../ignored\" }\n",
                ),
                (".hidden/src/lib.rs", ""),
                ("ignored/Cargo.toml", MANIFEST.1),
                (
                    "ignored/src/lib.rs",
                    "#[path = \"../../src/app.rs\"] mod mirror;",
                ),
            ],
        ),
        (
            "[target.'cfg(any())'.build-dependencies]\na = { path = \"ignored\" }\n",
            vec![
                ("ignored/Cargo.toml", MANIFEST.1),
                (
                    "ignored/src/lib.rs",
                    "#[path = \"../../src/app.rs\"] mod mirror;",
                ),
            ],
        ),
        (
            "[workspace]\nmembers = []\n[workspace.dependencies]\na = { path = \"ignored\" }\n",
            vec![
                ("ignored/Cargo.toml", MANIFEST.1),
                (
                    "ignored/src/lib.rs",
                    "#[path = \"../../src/app.rs\"] mod mirror;",
                ),
            ],
        ),
        (
            "[patch.crates-io]\na = { path = \"ignored\" }\n",
            vec![
                ("ignored/Cargo.toml", MANIFEST.1),
                ("ignored/src/lib.rs", "include!(\"../../src/app.rs\");"),
            ],
        ),
        (
            "[replace]\n'a:0.1.0' = { path = \"ignored\" }\n",
            vec![
                ("ignored/Cargo.toml", MANIFEST.1),
                ("ignored/src/lib.rs", "include!(\"../../src/app.rs\");"),
            ],
        ),
    ] {
        let manifest = format!("{}{manifest}", MANIFEST.1);
        let mut files = base();
        files.retain(|(path, _)| *path != "Cargo.toml");
        files.extend([
            ("Cargo.toml", manifest.as_str()),
            (".gitignore", ".hidden/\nignored/\ncrates/\n"),
        ]);
        files.extend(extra);
        assert_eq!(site_in(&files, "src/app.rs"), None, "{manifest}");
    }
    // 閉包から参照されない無視対象の独立 package は保証範囲に含めない。
    let mut files = base();
    files.extend([
        (".gitignore", "ignored/\n"),
        ("ignored/Cargo.toml", MANIFEST.1),
        ("ignored/src/lib.rs", "include!(\"../../src/app.rs\");"),
    ]);
    assert_eq!(
        site_in(&files, "src/app.rs"),
        Some(PathSite::ModuleFunction(Rc::from("src/engine.rs")))
    );
}

#[test]
fn removed_rust_layout_resolves_workspace_edition_and_implicit_ancestors() {
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\".hidden\"]\n[workspace.package]\nedition = \"2021\"\n",
        ),
        (
            ".hidden/Cargo.toml",
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
        ),
        (".hidden/src/lib.rs", "mod engine; mod app;"),
        (".hidden/src/engine.rs", "pub fn run() {}"),
        (".hidden/src/app.rs", "pub fn f() { crate::engine::run(); }"),
    ];
    assert_eq!(
        site_in(&files, ".hidden/src/app.rs"),
        Some(PathSite::ModuleFunction(Rc::from(".hidden/src/engine.rs")))
    );
    // package の暗黙の workspace 探索でも、無視対象の member を入力に加える。
    let files = [
        (".gitignore", "Cargo.toml\nignored/\n"),
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"member\", \"ignored\"]\n",
        ),
        ("member/Cargo.toml", MANIFEST.1),
        ("member/src/lib.rs", "mod engine; mod app;"),
        ("member/src/engine.rs", "pub fn run() {}"),
        ("member/src/app.rs", "pub fn f() { crate::engine::run(); }"),
        ("ignored/Cargo.toml", MANIFEST.1),
        (
            "ignored/src/lib.rs",
            "#[path = \"../../member/src/app.rs\"] mod mirror;",
        ),
    ];
    assert_eq!(site_in(&files, "member/src/app.rs"), None);
}

#[test]
fn removed_rust_layout_keeps_incomplete_inputs_and_config_overrides() {
    for (path, source) in [
        (
            "Cargo.toml",
            "[package]\nname = \"a\"\nedition = \"2021\"\n[dependencies]\na = { path = \"../outside\" }\n",
        ),
        (
            "Cargo.toml",
            "[package]\nname = \"a\"\nedition = \"2021\"\n[workspace]\nmembers = [\"**\"]\n",
        ),
        (
            "Cargo.toml",
            "[package]\nname = \"a\"\nedition = \"2021\"\n[workspace]\nmembers = [\"missing\"]\n",
        ),
        (
            "Cargo.toml",
            "[package]\nname = \"a\"\nedition = \"2021\"\n[[bin]]\nname = \"a\"\npath = \"missing\"\n",
        ),
        (".cargo/config.toml", "include = \"local.toml\"\n"),
        (".cargo/config.toml", "paths = [\"ignored\"]\n"),
        (
            ".cargo/config.toml",
            "[patch.crates-io]\na = { path = \"ignored\" }\n",
        ),
        (
            ".cargo/config",
            "[source.crates-io]\nreplace-with = \"other\"\n",
        ),
        ("src/engine.rs", "pub fn run( {}"),
    ] {
        let mut files = base();
        files.retain(|(existing, _)| *existing != path);
        files.push((path, source));
        assert_eq!(site_in(&files, "src/app.rs"), None, "{path}: {source}");
    }
    // 配置が証明不能でも、ファイル内で閉じる型の証拠は使う。
    assert_eq!(
        site_in(
            &[
                (".cargo/config.toml", "include = \"other.toml\""),
                ("local.rs", "struct Engine; fn f() { Engine::run(); }")
            ],
            "local.rs"
        ),
        Some(PathSite::Associated)
    );
}

#[test]
fn removed_rust_layout_audits_all_conditional_and_ambiguous_edges() {
    for extra in [
        vec![
            (
                "src/lib.rs",
                "mod engine; mod app; #[cfg(any())] mod generated;",
            ),
            ("src/generated.rs", "#[path = \"app.rs\"] mod mirror;"),
        ],
        vec![
            ("src/lib.rs", "mod engine; mod app; mod generated;"),
            ("src/generated.rs", ""),
            ("src/generated/mod.rs", "include!(\"../app.rs\");"),
        ],
        vec![(
            "src/main.rs",
            "#[cfg(any())] mod app; mod engine; fn main() {}",
        )],
        vec![(
            "src/main.rs",
            "mod app; mod engine { pub fn run() {} } fn main() {}",
        )],
        vec![("src/main.rs", "mount!(); fn main() {}")],
        vec![
            ("src/lib.rs", "mod engine; mod app; mod cached;"),
            (".gitignore", "src/cached/\n"),
            (
                "src/cached/CACHEDIR.TAG",
                "Signature: 8a477f597d28d172789f06886806bc55",
            ),
            ("src/cached/mod.rs", "#[path = \"../app.rs\"] mod mirror;"),
        ],
    ] {
        let mut files = base();
        for (path, source) in extra {
            files.retain(|(existing, _)| *existing != path);
            files.push((path, source));
        }
        assert_eq!(site_in(&files, "src/app.rs"), None, "{files:?}");
    }
    // root と普通の mod の兼用でも、全所属の証拠が同じなら分類できる。
    assert_eq!(
        site_in(
            &[
                MANIFEST,
                (
                    "src/lib.rs",
                    "pub mod engine; pub fn f() { crate::engine::run(); }"
                ),
                ("src/main.rs", "mod lib; mod engine; fn main() {}"),
                ("src/engine.rs", "pub fn run() {}"),
                ("src/lib/engine.rs", "pub fn run() {}"),
            ],
            "src/lib.rs"
        ),
        Some(PathSite::ModuleFunction(Rc::from("src/engine.rs")))
    );
}

#[test]
fn removed_rust_layout_tracks_inline_parents_without_repeating_directories() {
    assert_eq!(
        site_in(
            &[
                MANIFEST,
                ("src/lib.rs", "mod outer { pub mod engine; pub mod main; }"),
                ("src/outer/engine.rs", "pub fn run() {}"),
                ("src/outer/main.rs", "mod nested { pub mod child; }"),
                (
                    "src/outer/main/nested/child.rs",
                    "pub fn f() { super::super::super::engine::run(); }"
                ),
            ],
            "src/outer/main/nested/child.rs"
        ),
        Some(PathSite::ModuleFunction(Rc::from("src/outer/engine.rs")))
    );
    assert_eq!(
        site_in(
            &[
                MANIFEST,
                ("src/lib.rs", "mod helper { pub mod lib; }"),
                (
                    "src/helper/lib.rs",
                    "mod engine; pub fn f() { self::engine::run(); }"
                ),
                ("src/helper/lib/engine.rs", "pub fn run() {}"),
            ],
            "src/helper/lib.rs"
        ),
        Some(PathSite::ModuleFunction(Rc::from(
            "src/helper/lib/engine.rs"
        )))
    );
}

#[test]
fn removed_rust_layout_audits_macro_directories_and_dynamic_templates() {
    for source in [
        "macro_rules! mount { ($a:ident, $p:literal) => { #[$a = $p] mod mirror; }; } mount!(path, \"../src/app.rs\");",
        "macro_rules! mount { ($a:meta) => { #[$a] mod mirror; }; }",
        "macro_rules! mount { ($($a:tt)*) => { $($a)* }; }",
        "macro_rules! mount { ($m:ident) => { $m!(); }; }",
        "mount!();",
    ] {
        let mut files = base();
        files.extend([
            ("examples/probe.rs", source),
            (
                "examples/hidden/shared.rs",
                "#[path = \"../../src/app.rs\"] mod mirror;",
            ),
        ]);
        assert_eq!(site_in(&files, "src/app.rs"), None, "{source}");
    }
    // 未解決マクロの探索ディレクトリ外にある独立ファイルは所属を増やさない。
    let mut files = base();
    files.push((
        "examples/probe.rs",
        "macro_rules! m { ($x:expr) => { $x }; } fn main() { let _ = m!(1); }",
    ));
    assert_eq!(
        site_in(&files, "src/app.rs"),
        Some(PathSite::ModuleFunction(Rc::from("src/engine.rs")))
    );
}

#[cfg(windows)]
#[test]
fn removed_rust_layout_indexes_windows_case_shared_files() {
    assert_eq!(
        site_in(
            &[
                MANIFEST,
                ("src/lib.rs", "mod App; mod engine;"),
                (
                    "src/main.rs",
                    "mod app; mod engine { pub fn run() {} } fn main() {}"
                ),
                ("src/engine.rs", "pub fn run() {}"),
                ("src/App.rs", "pub fn f() { super::engine::run(); }"),
            ],
            "src/App.rs"
        ),
        None
    );
}

#[test]
fn removed_rust_layout_node_assumptions_match_the_grammar() {
    for kind in [
        "macro_rule",
        "token_tree_pattern",
        "token_binding_pattern",
        "fragment_specifier",
        "metavariable",
    ] {
        tree_sitter::Query::new(&LangId::Rust.ts_language(), &format!("({kind}) @n")).unwrap();
    }
    for (source, expected) in [
        (
            "macro_rules! m { ($a:ident, $p:literal) => { #[$a = $p] mod x; }; }",
            true,
        ),
        ("macro_rules! m { ($x:tt) => { $x }; }", true),
        ("macro_rules! m { ($x:expr) => { $x }; }", false),
        ("macro_rules! m { ($m:ident) => { $m!(); }; }", true),
    ] {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        assert!(super::super::rust_ref_sites::clean_tree(tree.root_node()));
        assert_eq!(
            layout::dynamic_macro_template(tree.root_node(), source.as_bytes()),
            expected,
            "{source}"
        );
    }
}

#[test]
fn removed_rust_paths_summarize_different_qualifiers_once_per_block() {
    let count = 2_000;
    let mut source = String::new();
    for index in 0..count {
        source.push_str(&format!("struct Type{index};\n"));
    }
    for _ in 0..count {
        source.push_str("// comment before one shared item\n");
    }
    source.push_str("#[allow(unused)]\nfn f() {\n");
    for index in 0..count {
        source.push_str(&format!("Type{index}::run();\n"));
    }
    source.push('}');
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let mut ids = HashMap::new();
    let scopes = FileScopes::collect(tree.root_node(), source.as_bytes(), Some(&mut ids)).unwrap();
    let file = RefFile {
        path: Rc::from("local.rs"),
        memo: NodeMemo::for_tree(tree.root_node(), source.as_bytes(), scopes.policy),
        scopes: Rc::new(scopes),
        ids,
    };
    let resolver = RustPathResolver::new(".");
    let mut proven = 0;
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        tree.root_node(),
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            if node.kind() == "identifier" && node.utf8_text(source.as_bytes()) == Ok("run") {
                assert_eq!(
                    resolver.classify(&file, node, source.as_bytes(), "run"),
                    Some(PathSite::Associated)
                );
                proven += 1;
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    assert_eq!(proven, count);
    assert_eq!(file.memo.blocks.borrow().len(), 1);
    assert_eq!(file.memo.generics.borrow().len(), 1);
    assert!(resolver.layout.get().is_none());
}

#[test]
fn removed_rust_layout_bounds_memberships_and_source_bytes() {
    for count in [128, 129] {
        let mut owned = vec![
            ("shared/entry".to_owned(), "mod engine; mod app;".to_owned()),
            ("shared/engine.rs".to_owned(), "pub fn run() {}".to_owned()),
            (
                "shared/app.rs".to_owned(),
                "pub fn f() { crate::engine::run(); }".to_owned(),
            ),
        ];
        for index in 0..count {
            owned.push((
                format!("pkg{index}/Cargo.toml"),
                format!("{}[lib]\npath = \"../shared/entry\"\n", MANIFEST.1),
            ));
        }
        let files: Vec<_> = owned
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect();
        let expected =
            (count == 128).then(|| PathSite::ModuleFunction(Rc::from("shared/engine.rs")));
        assert_eq!(site_in(&files, "shared/app.rs"), expected, "{count}");
    }
    let oversized = format!("pub fn run() {{}}{}", " ".repeat(2 * 1024 * 1024));
    let mut files = base();
    files.retain(|(path, _)| *path != "src/engine.rs");
    files.push(("src/engine.rs", &oversized));
    assert_eq!(site_in(&files, "src/app.rs"), None);
}

#[test]
fn removed_rust_layout_follows_explicit_workspace_from_ignored_manifest() {
    assert_eq!(
        site_in(
            &[
                (".gitignore", "Cargo.toml\nignored/\n"),
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"member\", \"ignored\"]\n[workspace.package]\nedition = \"2021\"\n"
                ),
                (
                    "member/Cargo.toml",
                    "[package]\nname = \"member\"\nworkspace = \"..\"\nedition.workspace = true\n"
                ),
                ("member/src/lib.rs", "mod engine; mod app;"),
                ("member/src/engine.rs", "pub fn run() {}"),
                ("member/src/app.rs", "pub fn f() { crate::engine::run(); }"),
                ("ignored/Cargo.toml", MANIFEST.1),
                (
                    "ignored/src/lib.rs",
                    "include!(\"../../member/src/app.rs\");"
                ),
            ],
            "member/src/app.rs"
        ),
        None
    );
}

#[test]
fn removed_rust_layout_explicit_lib_replaces_the_implicit_root() {
    let manifest = format!("{}[lib]\npath = \"entry\"\n", MANIFEST.1);
    assert_eq!(
        site_in(
            &[
                ("Cargo.toml", &manifest),
                ("entry", "mod engine; pub fn f() { crate::engine::run(); }"),
                ("engine.rs", "pub fn run() {}"),
                ("src/lib.rs", "include!(\"../entry\");"),
            ],
            "entry"
        ),
        Some(PathSite::ModuleFunction(Rc::from("engine.rs")))
    );
}

#[test]
fn removed_rust_layout_preserves_target_kinds_for_one_physical_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        format!("{}[lib]\npath = \"build.rs\"\n", MANIFEST.1),
    )
    .unwrap();
    std::fs::write(dir.path().join("build.rs"), "fn main() {}").unwrap();
    let layout = layout::Layout::build(dir.path()).unwrap();
    let contexts = layout.contexts("build.rs").unwrap();
    assert_eq!(contexts.len(), 2);
    let kinds: std::collections::BTreeSet<_> = contexts
        .iter()
        .map(|context| layout.crate_inputs[context].kind)
        .collect();
    assert_eq!(
        kinds,
        [layout::TargetKind::Library, layout::TargetKind::BuildScript]
            .into_iter()
            .collect()
    );
    assert!(layout.packages.contains_key(""));
    assert_eq!(
        layout.crate_inputs[&layout.library_roots[""]].kind,
        layout::TargetKind::Library
    );
    assert!(
        contexts
            .iter()
            .all(|context| layout.crate_inputs[context].package.is_empty())
    );
}

#[test]
fn removed_rust_paths_keep_attribute_policies_separate() {
    let source =
        "#[cfg(any())]\n#[derive(Clone)]\nstruct Engine;\n#[cfg(any())]\nfn f() { Engine::run(); }";
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let policy = AttributePolicy::for_tree(tree.root_node(), source.as_bytes());
    let summaries = AttributeSummaries::collect(tree.root_node(), source.as_bytes(), policy);
    let mut cursor = tree.root_node().walk();
    let items: Vec<_> = tree
        .root_node()
        .named_children(&mut cursor)
        .filter(|node| matches!(node.kind(), "struct_item" | "function_item"))
        .collect();
    assert_eq!(items.len(), 2);
    let allowance = summaries.outer(items[0]);
    assert!(!allowance.item);
    assert!(!allowance.typ);
    assert!(allowance.tokens);
    let allowance = summaries.outer(items[1]);
    assert!(!allowance.item);
    assert!(allowance.tokens);
}

#[test]
fn removed_rust_paths_keep_unicode_import_aliases_blocking() {
    let import = "use crate::launcher as cafe\u{301};";
    let tree = parser::parse_source(import.as_bytes(), LangId::Rust).unwrap();
    assert!(identifier_tokens(tree.root_node(), import.as_bytes()).contains("cafe\u{301}"));
    for (module, alias, call) in [
        ("cafe\u{301}", "cafe\u{301}", "cafe\u{301}"),
        ("caf\u{e9}", "cafe\u{301}", "caf\u{e9}"),
        ("K", "\u{212a}", "K"),
    ] {
        let source = format!(
            "mod {module} {{ pub fn run() {{}} }} pub fn f() {{ use crate::launcher as {alias}; {call}::run(); }}"
        );
        assert_eq!(
            site_in(&[("src/local.rs", &source)], "src/local.rs"),
            None,
            "{source}"
        );
    }
    assert_eq!(
        site_in(
            &[(
                "src/local.rs",
                "mod K { pub fn run() {} } pub fn f() { mod \u{212a} { pub use crate::launcher::run; } K::run(); }"
            )],
            "src/local.rs"
        ),
        None
    );
    assert_eq!(
        site_in(
            &[(
                "src/local.rs",
                "mod cafe\u{301} { pub fn run() {} } pub fn f() { cafe\u{301}::run(); }"
            )],
            "src/local.rs"
        ),
        Some(PathSite::ModuleFunction(Rc::from("src/local.rs")))
    );
}

#[test]
fn removed_rust_layout_compresses_audits_at_component_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in [
        MANIFEST,
        (
            "src/lib.rs",
            "mod engine; mod generated { pub mod app; } mod generated_extra { pub mod app; }",
        ),
        ("src/engine.rs", "pub fn run() {}"),
        (
            "src/main.rs",
            "mod generated { mount!(); mod nested { mount!(); } } fn main() {}",
        ),
        (
            "src/generated/app.rs",
            "pub fn f() { crate::engine::run(); }",
        ),
        ("src/generated/nested/harmless.rs", ""),
        (
            "src/generated_extra/app.rs",
            "pub fn f() { crate::engine::run(); }",
        ),
    ] {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let layout = layout::Layout::build(dir.path()).unwrap();
    assert_eq!(layout.audited_directory_count(), 1);
    for _ in 0..1_000 {
        assert!(layout.contexts("src/generated/app.rs").is_none());
        assert!(layout.contexts("src/generated_extra/app.rs").is_some());
    }
}

#[cfg(any(unix, windows))]
#[test]
fn removed_rust_layout_normalizes_ancestor_aliases_but_rejects_root_links() {
    fn link_directory(target: &std::path::Path, link: &std::path::Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            let output = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "New-Item -ItemType Junction -Path $env:ASTRO_SIGHT_TEST_JUNCTION_PATH -Target ([System.Management.Automation.WildcardPattern]::Escape($env:ASTRO_SIGHT_TEST_JUNCTION_TARGET)) -ErrorAction Stop | Out-Null",
                ])
                .env("ASTRO_SIGHT_TEST_JUNCTION_PATH", link)
                .env("ASTRO_SIGHT_TEST_JUNCTION_TARGET", target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_ne!(
                std::fs::symlink_metadata(link).unwrap().file_attributes() & 0x400,
                0
            );
        }
    }

    let temp_parent = tempfile::tempdir().unwrap();
    let special_parent = temp_parent.path().join("a&b[layout]");
    std::fs::create_dir_all(&special_parent).unwrap();
    let dir = tempfile::tempdir_in(&special_parent).unwrap();
    let physical = dir.path().join("physical/nested");
    let project = physical.join("project");
    for (path, source) in base() {
        let path = project.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let logical_parent = dir.path().join("logical");
    std::fs::create_dir_all(&logical_parent).unwrap();
    let parent_alias = logical_parent.join("parent_alias");
    link_directory(&physical, &parent_alias);
    let aliased_project = parent_alias.join("project");
    std::fs::create_dir_all(project.join(".cargo")).unwrap();
    std::fs::write(
        project.join(".cargo/config.toml"),
        "[net]\noffline = true\n",
    )
    .unwrap();
    let layout = layout::Layout::build(&aliased_project).unwrap();
    assert!(layout.contexts("src/app.rs").is_some());
    let source = "pub fn f() { super::engine::run(); }";
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    assert!(
        RustPathResolver::new(aliased_project.to_str().unwrap())
            .positions(tree.root_node(), source.as_bytes(), "run", "src/app.rs")
            .values()
            .any(|site| *site == PathSite::ModuleFunction(Rc::from("src/engine.rs")))
    );

    let dependency_project = physical.join("dependencies");
    let dependency_source = "pub fn f() { beta::items::run(); }";
    for (path, source) in [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"alpha\"]\nresolver = \"2\"\n",
        ),
        (
            "alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nbeta = { path = \"../beta\" }\n",
        ),
        ("alpha/src/lib.rs", dependency_source),
        (
            "beta/Cargo.toml",
            "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("beta/src/lib.rs", "pub mod items { pub fn run() {} }"),
    ] {
        let path = dependency_project.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let aliased_dependency_project = parent_alias.join("dependencies");
    let dependency_layout = layout::Layout::build(&aliased_dependency_project).unwrap();
    assert_eq!(
        dependency_layout.package_at("alpha", "../beta"),
        Some(Rc::from("beta"))
    );
    let resolves_dependency = || {
        let tree = parser::parse_source(dependency_source.as_bytes(), LangId::Rust).unwrap();
        RustPathResolver::new(aliased_dependency_project.to_str().unwrap())
            .positions(
                tree.root_node(),
                dependency_source.as_bytes(),
                "run",
                "alpha/src/lib.rs",
            )
            .values()
            .any(|site| *site == PathSite::ModuleFunction(Rc::from("beta/src/lib.rs")))
    };
    assert!(resolves_dependency());
    let physical_ancestor = physical.parent().unwrap();
    std::fs::create_dir_all(physical_ancestor.join(".cargo")).unwrap();
    let physical_config = physical_ancestor.join(".cargo/config.toml");
    std::fs::write(
        &physical_config,
        "[build]\nrustc-wrapper = \"other-rustc\"\n",
    )
    .unwrap();
    assert!(layout::Layout::build(&aliased_dependency_project).is_some());
    assert!(!resolves_dependency());
    std::fs::remove_file(physical_config).unwrap();
    assert!(resolves_dependency());

    std::fs::create_dir_all(logical_parent.join(".cargo")).unwrap();
    let config = logical_parent.join(".cargo/config.toml");
    std::fs::write(&config, "paths = [\"elsewhere\"]\n").unwrap();
    assert!(layout::Layout::build(&aliased_project).is_none());
    assert!(layout::Layout::build(&project).is_some());
    std::fs::remove_file(config).unwrap();
    let manifest = logical_parent.join("Cargo.toml");
    std::fs::write(&manifest, "[workspace]\nmembers = []\n").unwrap();
    assert!(layout::Layout::build(&aliased_project).is_none());
    assert!(layout::Layout::build(&project).is_some());
    std::fs::remove_file(manifest).unwrap();

    let config_project = physical.join("config_project");
    for (path, source) in base() {
        let path = config_project.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let external_config = dir.path().join("external_config");
    std::fs::create_dir_all(&external_config).unwrap();
    std::fs::write(
        external_config.join("config.toml"),
        "[net]\noffline = true\n",
    )
    .unwrap();
    link_directory(&external_config, &config_project.join(".cargo"));
    assert!(layout::Layout::build(&parent_alias.join("config_project")).is_none());

    let root_alias = dir.path().join("root_alias");
    link_directory(&project, &root_alias);
    for root in [
        root_alias.clone(),
        root_alias.join("."),
        std::path::PathBuf::from(format!(
            "{}{}",
            root_alias.display(),
            std::path::MAIN_SEPARATOR
        )),
    ] {
        assert!(layout::Layout::build(&root).is_none(), "{}", root.display());
    }

    let external = dir.path().join("external");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("mod.rs"), "pub fn run() {}").unwrap();
    link_directory(&external, &project.join("src/redirected"));
    std::fs::write(
        project.join("src/lib.rs"),
        "mod engine; mod app; mod redirected;",
    )
    .unwrap();
    assert!(layout::Layout::build(&aliased_project).is_none());
}
