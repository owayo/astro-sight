//! Rust の自由関数と関連メンバーの位置を区別する API 判定。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

fn api_after(files: &[(&str, &str)], changes: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut all = vec![
        (
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", "pub mod api;\npub mod client;\n"),
    ];
    all.extend_from_slice(files);
    git_commit_files(repo, &all, "base");
    for (path, source) in changes {
        fs::write(repo.join(path), source).unwrap();
    }
    detect_api_changes_from_worktree(repo)
}

#[test]
fn rust_reference_sites_updated_free_function_ignores_associated_members() {
    let old = "pub fn put(count: u32) -> u32 {\n count * 2\n}\n";
    let new = "pub fn put(count: u32, label: &str) -> u32 {\n count * 2 + label.len() as u32\n}\n";
    let client = "use crate::api;\npub struct Client;\nimpl Client {\n pub fn put(&self, count: u32) -> u32 {\n  api::put(count)\n }\n}\npub fn handle(client: &Client) -> u32 { client.put(3) }\n";
    let updated = client.replace("api::put(count)", "api::put(count, \"a\")");
    let api = api_after(
        &[("src/api.rs", old), ("src/client.rs", client)],
        &[("src/api.rs", new), ("src/client.rs", &updated)],
    );
    assert!(api.modified.iter().all(|s| s.name != "put"), "{api:?}");
    assert!(
        api.modified_closed_in_diff.iter().any(|s| s.name == "put"),
        "{api:?}"
    );
}

#[test]
fn rust_reference_sites_removed_free_function_ignores_associated_members() {
    let client = "pub struct Engine;\nimpl Engine {\n pub fn run(&self) -> u32 { 1 }\n}\npub fn drive(engine: &Engine) -> u32 { engine.run() }\n";
    let api = api_after(
        &[
            ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
            ("src/client.rs", client),
        ],
        &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
    );
    assert!(api.removed.iter().all(|s| s.name != "run"), "{api:?}");
    assert!(api.removed_dead.iter().any(|s| s.name == "run"), "{api:?}");
}

#[test]
fn rust_reference_sites_target_inner_attributes_disable_free_function_proofs() {
    let old = "pub fn put(n: u32) -> u32 { #![rewrite] n }\n";
    let new = "pub fn put(n: u32, extra: u32) -> u32 { #![rewrite] n + extra }\n";
    let client = "pub struct Client; impl Client { pub fn put(&self) {} } pub fn call(c: &Client) { c.put(); crate::api::put(1); }\n";
    let updated = client.replace("crate::api::put(1)", "crate::api::put(1, 2)");
    let api = api_after(
        &[("src/api.rs", old), ("src/client.rs", client)],
        &[("src/api.rs", new), ("src/client.rs", &updated)],
    );
    assert!(api.modified.iter().any(|s| s.name == "put"), "{api:?}");
    assert!(
        api.modified_closed_in_diff.iter().all(|s| s.name != "put"),
        "{api:?}"
    );
    let api = api_after(
        &[
            ("src/api.rs", old),
            (
                "src/client.rs",
                "pub struct Client; impl Client { pub fn put(&self) {} } pub fn call(c: &Client) { c.put(); }\n",
            ),
        ],
        &[("src/api.rs", "pub fn launch() -> u32 { 1 }\n")],
    );
    assert!(api.removed.iter().any(|s| s.name == "put"), "{api:?}");
    assert!(api.removed_dead.iter().all(|s| s.name != "put"), "{api:?}");
}

#[test]
fn rust_reference_sites_closure_preserves_unupdated_and_ambiguous_uses() {
    let old = "pub fn put(n: u32) -> u32 {\n n\n}\n";
    let new = "pub fn put(n: u32, extra: u32) -> u32 {\n n + extra\n}\n";
    let caller_old = "pub fn call() -> u32 { crate::api::put(1) }\n";
    let caller_new = "pub fn call() -> u32 { crate::api::put(1, 2) }\n";
    for extra in [
        "pub struct Client; impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { c.put(1); crate::api::put(1); }\n",
        "use crate::api::put; pub fn call() { put(1); }\n",
        "use crate::api::put as save; pub fn call() { save(1); }\n",
        "pub fn call() { let f = crate::api::put; f(1); }\n",
        "pub fn put(n: u32) -> u32 { n }\n",
        "impl Client { fn outer() { fn put(n: u32) -> u32 { n } } }\n",
        "pub struct Client { pub put: fn(u32) -> u32 } impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { (c.put)(1); }\n",
        "pub struct Client { pub put: fn(u32) -> u32 } impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { let f = c.put; f(1); }\n",
        "macro_rules! m { ($x:expr) => { $x }; } pub fn call(c: &Client) { m!(c.put(1)); }\n",
        "#[rewrite] impl Client { pub fn put(&self, n: u32) -> u32 { n } }\n",
        "#![rewrite]\npub struct Client; impl Client { pub fn put(&self, n: u32) -> u32 { n } } pub fn call(c: &Client) { c.put(1); }\n",
        "pub fn malformed( { c.put(1); }\n",
    ] {
        let api = api_after(
            &[
                ("src/api.rs", old),
                ("src/client.rs", caller_old),
                ("extra.rs", extra),
            ],
            &[("src/api.rs", new), ("src/client.rs", caller_new)],
        );
        assert!(
            api.modified.iter().any(|s| s.name == "put"),
            "{extra}: {api:?}"
        );
        assert!(
            api.modified_closed_in_diff.iter().all(|s| s.name != "put"),
            "{extra}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_closure_handles_members_in_target_and_other_crates() {
    let methods = "pub struct Client;\nimpl Client { pub fn put(&self, n: u32) -> u32 { n } }\n";
    for method_path in ["src/api.rs", "src/client.rs", "other/src/lib.rs"] {
        let old = format!(
            "pub fn put(n: u32) -> u32 {{\n n\n}}\n{}",
            if method_path == "src/api.rs" {
                methods
            } else {
                ""
            }
        );
        let new = old.replace("put(n: u32)", "put(n: u32, extra: u32)");
        let client_old = format!(
            "{}pub fn call(c: &Client) -> u32 {{ c.put(crate::api::put(1)) }}\n",
            if method_path == "src/client.rs" {
                methods
            } else {
                ""
            }
        );
        let client_new = client_old.replace("crate::api::put(1)", "crate::api::put(1, 2)");
        let mut files = vec![
            ("src/api.rs", old.as_str()),
            ("src/client.rs", client_old.as_str()),
        ];
        if method_path == "other/src/lib.rs" {
            files.push((method_path, methods));
            files.push((
                "other/Cargo.toml",
                "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ));
        }
        let api = api_after(
            &files,
            &[("src/api.rs", &new), ("src/client.rs", &client_new)],
        );
        assert!(
            api.modified_closed_in_diff.iter().any(|s| s.name == "put"),
            "{method_path}: {api:?}"
        );
        assert!(
            api.modified.iter().all(|s| s.name != "put"),
            "{method_path}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_removal_preserves_function_values_and_real_uses() {
    let methods = "pub struct Engine { pub run: fn() -> u32 }\nimpl Engine { pub fn run(&self) -> u32 { 1 } }\n";
    for usage in [
        "pub fn drive() -> u32 { crate::api::run() }\n",
        "use crate::api::run; pub fn drive() -> u32 { run() }\n",
        "use crate::api::run as go; pub fn drive() -> u32 { go() }\n",
        "pub fn drive() { let f = crate::api::run; f(); }\n",
        "pub fn drive(e: &Engine) -> u32 { (e.run)() }\n",
        "pub fn drive(e: &Engine) { let f = e.run; f(); }\n",
        "pub fn drive(e: &Engine) { consume(e.run); }\n",
        "pub fn get(e: &Engine) -> fn() -> u32 { e.run }\n",
        "pub fn get(e: &Engine) -> (fn() -> u32,) { (e.run,) }\n",
        "pub fn get(e: &Engine) -> [fn() -> u32; 1] { [e.run] }\n",
        "#[rewrite] pub fn get(e: &Engine) -> fn() -> u32 { e.run }\n",
        "pub struct Count { pub run: u32 } pub fn get(e: &Engine) -> fn() -> u32 { e.run }\n",
        "pub struct Count { pub run: u32 } pub fn get(e: &Count) -> fn() -> u32 { let e = Engine { run: fallback }; e.run }\n",
        "pub fn drive(e: &Engine) { e.run::<u32>(); }\n",
        "pub fn drive(e: &Engine) { m!(e.run()); }\n",
        "#[rewrite] pub fn drive(e: &Engine) { e.run(); }\n",
        "pub mod nested { #![rewrite] pub fn drive(e: &super::Engine) { e.run(); } }\n",
        "pub fn drive(e: &Engine) { #![rewrite] e.run(); }\n",
        "pub fn malformed( { e.run(); }\n",
    ] {
        let client = format!("{methods}{usage}");
        let api = api_after(
            &[
                ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
                ("src/client.rs", &client),
            ],
            &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
        );
        assert!(
            api.removed.iter().any(|s| s.name == "run"),
            "{usage}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().all(|s| s.name != "run"),
            "{usage}: {api:?}"
        );
    }
}

/// 型 `Engine` を修飾部に持つ `Engine::run` は型の関連項目 (ここでは `&self` のメソッド) を
/// 指すパスで、フィールドの関数ポインタ (`e.run` でしか読めない) にも、消したモジュールの
/// 自由関数にもならない。以前は修飾部を確かめずに残存参照と数えていた (Issue #110)。
/// `Engine::run()` は receiver の引数が足りずコンパイルできないが、パスの指す先は同じ。
#[test]
fn rust_reference_sites_removal_ignores_type_paths_beside_function_fields() {
    let methods = "pub struct Engine { pub run: fn() -> u32 }\nimpl Engine { pub fn run(&self) -> u32 { 1 } }\n";
    for usage in [
        "pub fn drive() { Engine::run(); }\n",
        "pub fn drive(e: &Engine) -> u32 { Engine::run(e) }\n",
        "pub fn drive() -> fn(&Engine) -> u32 { Engine::run }\n",
    ] {
        let client = format!("{methods}{usage}");
        let api = api_after(
            &[
                ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
                ("src/client.rs", &client),
            ],
            &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
        );
        assert!(
            api.removed.iter().all(|s| s.name != "run"),
            "{usage}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().any(|s| s.name == "run"),
            "{usage}: {api:?}"
        );
    }
}

/// `src/launcher.rs` の自由関数 `run` をファイルごと消し、`lib.rs` から `pub mod launcher;` を
/// 消す。`modules` は変更後の `lib.rs`、`files` は変えないファイル。
fn api_after_removing_launcher(modules: &str, files: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let lib = format!("pub mod launcher;\n{modules}");
    let mut all = vec![
        (
            "Cargo.toml",
            "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", lib.as_str()),
        ("src/launcher.rs", "pub fn run() -> u32 {\n 7\n}\n"),
    ];
    all.extend_from_slice(files);
    git_commit_files(repo, &all, "base");
    fs::remove_file(repo.join("src/launcher.rs")).unwrap();
    fs::write(repo.join("src/lib.rs"), modules).unwrap();
    detect_api_changes_from_worktree(repo)
}

/// 修飾部や素の名前が、残るモジュールの自由関数か型の関連項目を指すと確かめられる参照は、
/// 消した自由関数の残存参照に数えない (Issue #110)。
#[test]
fn rust_reference_sites_removal_resolves_paths_to_surviving_items() {
    let modules = "pub mod engine;\npub mod app;\n";
    let engine = "pub fn run() -> u32 { 1 }\npub fn drive() -> u32 { run() }\n";
    let typed = "pub struct Engine;\nimpl Engine { pub fn run() -> u32 { 1 } }\n";
    for (engine, app) in [
        // 再現 A: 別モジュールの同名の自由関数へのパスと、同じモジュールの素の呼び出し
        (engine, "pub fn start() -> u32 { crate::engine::run() }\n"),
        (engine, "pub fn start() -> u32 { super::engine::run() }\n"),
        (
            engine,
            "pub fn start() -> fn() -> u32 { crate::engine::run }\n",
        ),
        (
            "pub fn run() -> u32 { 1 }\npub fn drive() -> u32 { self::run() }\npub mod inner { pub fn go() -> u32 { super::run() } }\n",
            "",
        ),
        // 再現 B: 同じモジュールが宣言する型の関連関数
        (
            "pub struct Engine;\nimpl Engine { pub fn run() -> u32 { 1 } }\npub fn drive() -> u32 { Engine::run() }\n",
            "",
        ),
        (
            "#[derive(Clone, Debug)]\npub struct Engine;\nimpl Engine { pub fn run() -> u32 { 1 } pub fn go() -> u32 { Self::run() + <Engine>::run() } }\n",
            "",
        ),
        (
            typed,
            "pub fn start() -> u32 { crate::engine::Engine::run() }\n",
        ),
    ] {
        let api =
            api_after_removing_launcher(modules, &[("src/engine.rs", engine), ("src/app.rs", app)]);
        assert!(
            api.removed.iter().all(|s| s.name != "run"),
            "{engine}{app}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().any(|s| s.name == "run"),
            "{engine}{app}: {api:?}"
        );
    }
    // lib と bin の両方があっても、参照元へ mod 宣言で届く crate root が 1 つなら解決する
    let api = api_after_removing_launcher(
        modules,
        &[
            ("src/engine.rs", engine),
            (
                "src/app.rs",
                "pub fn start() -> u32 { crate::engine::run() }\n",
            ),
            ("src/main.rs", "fn main() {\n    tool::app::start();\n}\n"),
        ],
    );
    assert!(api.removed_dead.iter().any(|s| s.name == "run"), "{api:?}");
    // 残る定義がインライン mod の中にある・同じファイルに同名のメソッドと属性付きの自由関数がある
    for (modules, files) in [
        (
            "pub mod engine;\npub mod app;\npub mod inner {\n    pub fn run() -> u32 { 1 }\n    pub fn go() -> u32 { run() }\n}\n",
            [
                (
                    "src/engine.rs",
                    "pub fn drive() -> u32 { crate::inner::run() }\n",
                ),
                (
                    "src/app.rs",
                    "pub fn start() -> u32 { super::inner::run() }\n",
                ),
            ],
        ),
        (
            modules,
            [
                (
                    "src/engine.rs",
                    "pub struct Engine;\nimpl Engine { pub fn run(&self) -> u32 { 1 } }\n#[inline]\npub fn run() -> u32 { 1 }\npub fn drive(e: &Engine) -> u32 { run() + e.run() }\n",
                ),
                (
                    "src/app.rs",
                    "pub fn start() -> u32 { crate::engine::run() }\n",
                ),
            ],
        ),
    ] {
        let api = api_after_removing_launcher(modules, &files);
        assert!(
            api.removed_dead.iter().any(|s| s.name == "run"),
            "{modules}{files:?}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_removal_accepts_matching_memberships_and_unrelated_roots() {
    for extra in [
        vec![("build.rs", "fn main() {}")],
        vec![("src/main.rs", "mod engine; mod app; fn main() {}")],
        vec![("src/scratch.rs", "include!(\"app.rs\");")],
    ] {
        let mut files = vec![
            ("src/engine.rs", "pub fn run() -> u32 { 1 }"),
            (
                "src/app.rs",
                "pub fn start() -> u32 { crate::engine::run() }",
            ),
        ];
        files.extend(extra);
        let api = api_after_removing_launcher("pub mod engine; pub mod app;", &files);
        assert!(
            api.removed_dead.iter().any(|symbol| symbol.name == "run"),
            "{files:?}: {api:?}"
        );
        assert!(
            api.removed.iter().all(|symbol| symbol.name != "run"),
            "{files:?}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_removal_preserves_unicode_import_aliases() {
    for (module, alias, call) in [
        ("cafe\u{301}", "cafe\u{301}", "cafe\u{301}"),
        ("caf\u{e9}", "cafe\u{301}", "caf\u{e9}"),
        ("K", "\u{212a}", "K"),
    ] {
        let source = format!(
            "mod {module} {{ pub fn run() -> u32 {{ 1 }} }} pub fn drive() -> u32 {{ use crate::launcher as {alias}; {call}::run() }}"
        );
        let api = api_after_removing_launcher("pub mod engine;", &[("src/engine.rs", &source)]);
        assert!(
            api.removed.iter().any(|symbol| symbol.name == "run"),
            "{source}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().all(|symbol| symbol.name != "run"),
            "{source}: {api:?}"
        );
    }
}

/// 変更後の `lib.rs`、`src/engine.rs`、変えないほかのファイル。
type RemovalCase<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

/// 修飾部や素の名前の指す先を確かめられない参照、消したモジュールを指す参照は、
/// 今と同じく消した自由関数の残存参照として api.rm に残す (Issue #110)。
#[test]
fn rust_reference_sites_removal_keeps_unresolved_and_removed_paths() {
    let modules = "pub mod engine;\npub mod app;\n";
    let engine = "pub fn run() -> u32 { 1 }\npub fn drive() -> u32 { run() }\n";
    let cases: &[RemovalCase<'_>] = &[
        // 消したモジュールを指すパスと、消した関数の import・別名
        (
            modules,
            engine,
            &[(
                "src/app.rs",
                "pub fn start() -> u32 { crate::launcher::run() }\n",
            )],
        ),
        (
            modules,
            engine,
            &[(
                "src/app.rs",
                "use crate::launcher::run;\npub fn start() -> u32 { run() }\n",
            )],
        ),
        (
            modules,
            engine,
            &[(
                "src/app.rs",
                "use crate::launcher as engine2;\npub fn start() -> u32 { engine2::run() }\n",
            )],
        ),
        // 先頭が宣言の無い名前 (外部 crate かもしれない)
        (
            modules,
            engine,
            &[("src/app.rs", "pub fn start() -> u32 { engine::run() }\n")],
        ),
        // マクロの中・属性付きの item・ブロックのグロブ import・cfg 付きの定義
        (
            modules,
            "pub fn run() -> u32 { 1 }\nmacro_rules! m { ($e:expr) => { $e }; }\npub fn drive() -> u32 { m!(run()) }\n",
            &[],
        ),
        (
            modules,
            "pub fn run() -> u32 { 1 }\n#[rewrite]\npub fn drive() -> u32 { run() }\n",
            &[],
        ),
        (
            modules,
            "pub fn run() -> u32 { 1 }\npub fn drive() -> u32 { use crate::app::*; run() }\n",
            &[],
        ),
        (
            modules,
            "#[cfg(test)]\npub fn run() -> u32 { 1 }\npub fn drive() -> u32 { run() }\n",
            &[],
        ),
        // 型名・モジュール名をブロックの import が隠す (呼び出しより後の import も)・ブロックの
        // derive が item を足し得る・修飾部の型が同じモジュールに無い
        (
            modules,
            "pub struct Engine;\npub fn drive() -> u32 { use crate::app as Engine; Engine::run() }\n",
            &[],
        ),
        (
            modules,
            "pub mod inner { pub fn run() -> u32 { 1 } }\npub fn drive() -> u32 { use crate::app as inner; inner::run() }\n",
            &[],
        ),
        (
            modules,
            "pub mod inner { pub fn run() -> u32 { 1 } }\npub fn drive() -> u32 { let n = inner::run(); use crate::app as inner; n }\n",
            &[],
        ),
        (
            modules,
            "pub fn run() -> u32 { 1 }\npub fn drive() -> u32 { #[derive(Inject)] struct Marker; run() }\n",
            &[],
        ),
        (
            modules,
            "pub fn drive() -> u32 { Engine::run() }\n",
            &[("src/app.rs", "pub struct Engine;\n")],
        ),
        // 同じファイルを別の crate やモジュールに置き得る `#[path]`・`include!`・暗黙の build script
        (
            "pub mod engine;\npub mod app;\n#[path = \"engine.rs\"]\nmod mirror;\n",
            engine,
            &[(
                "src/app.rs",
                "pub fn start() -> u32 { crate::engine::run() }\n",
            )],
        ),
        (
            "pub mod engine; pub mod app; mod extra;\n",
            engine,
            &[
                (
                    "src/app.rs",
                    "pub fn start() -> u32 { crate::engine::run() }\n",
                ),
                ("src/extra.rs", "include!(\"app.rs\");\n"),
            ],
        ),
        (
            modules,
            engine,
            &[
                (
                    "src/app.rs",
                    "pub fn start() -> u32 { crate::engine::run() }\n",
                ),
                (
                    "build.rs",
                    "#[path = \"src/app.rs\"] mod mirror; fn main() {}\n",
                ),
            ],
        ),
        // 別の root で同じ参照が別の宣言へ解決される
        (
            modules,
            engine,
            &[
                (
                    "src/app.rs",
                    "pub fn start() -> u32 { crate::engine::run() }\n",
                ),
                (
                    "src/main.rs",
                    "mod app; mod engine { pub fn run() -> u32 { 2 } } fn main() {}\n",
                ),
            ],
        ),
    ];
    for (modules, engine, files) in cases {
        let mut all = vec![("src/engine.rs", *engine)];
        all.extend_from_slice(files);
        if !all.iter().any(|(path, _)| *path == "src/app.rs") {
            all.push(("src/app.rs", ""));
        }
        let api = api_after_removing_launcher(modules, &all);
        assert!(
            api.removed.iter().any(|s| s.name == "run"),
            "{modules}{engine}{files:?}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().all(|s| s.name != "run"),
            "{modules}{engine}{files:?}: {api:?}"
        );
    }
}

#[test]
fn rust_reference_sites_removal_handles_multiple_members_and_keeps_method_removal() {
    let methods = "pub struct A; impl A { pub fn run(&self) -> u32 { 1 } }\npub struct B; impl B { pub fn run(&self) -> u32 { 2 } }\npub trait Runner { fn run(&self) -> u32; }\n";
    let api = api_after(
        &[
            ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
            ("src/client.rs", methods),
        ],
        &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
    );
    assert!(api.removed_dead.iter().any(|s| s.name == "run"), "{api:?}");
    let api = api_after(
        &[
            (
                "src/api.rs",
                "pub struct Old;\nimpl Old { pub fn run(&self) -> u32 { 7 } }\n",
            ),
            ("src/client.rs", methods),
        ],
        &[("src/api.rs", "pub struct Old;\n")],
    );
    assert!(api.removed.iter().any(|s| s.name == "Old.run"), "{api:?}");
    assert!(
        api.removed_dead.iter().all(|s| s.name != "Old.run"),
        "{api:?}"
    );
}

#[test]
fn rust_reference_sites_removal_distinguishes_other_crate_methods_and_free_calls() {
    for free_call in [false, true] {
        let consumer = format!(
            "pub struct Engine; impl Engine {{ pub fn run(&self) -> u32 {{ 1 }} }} pub fn drive(e: &Engine) -> u32 {{ e.run(){} }}\n",
            if free_call { " + demo::api::run()" } else { "" }
        );
        let api = api_after(
            &[
                ("src/api.rs", "pub fn run() -> u32 {\n 7\n}\n"),
                ("src/client.rs", ""),
                ("other/src/lib.rs", &consumer),
                (
                    "other/Cargo.toml",
                    "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ndemo = { path = \"../..\" }\n",
                ),
            ],
            &[("src/api.rs", "pub fn launch() -> u32 {\n 7\n}\n")],
        );
        assert_eq!(
            api.removed.iter().any(|s| s.name == "run"),
            free_call,
            "{api:?}"
        );
        assert_eq!(
            api.removed_dead.iter().any(|s| s.name == "run"),
            !free_call,
            "{api:?}"
        );
    }
}

const HAS_FLAG: &str = "pub fn has_flag(s: Option<&str>) -> bool {\n    s == Some(\"tasks\")\n}\n";

fn crate_manifest(name: &str, dependencies: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}"
    )
}

/// workspace のクレート alpha の `items::has_flag` を beta へ移し (alpha の items.rs は削除)、
/// 同じ名前の関数 `gamma_function` を gamma にも足す (Issue #115 の再現)。`alpha_lib` と
/// `alpha_dependencies` は変更後の alpha の lib.rs と依存、`extra` は変更後に書くファイル。
fn api_after_moving_to_other_crate(
    alpha_lib: &str,
    alpha_dependencies: &str,
    gamma_function: &str,
    extra: &[(&str, &str)],
) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let (alpha, beta, gamma) = (
        crate_manifest("alpha", ""),
        crate_manifest("beta", ""),
        crate_manifest("gamma", ""),
    );
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/gamma\"]\nresolver = \"2\"\n",
            ),
            ("crates/alpha/Cargo.toml", &alpha),
            (
                "crates/alpha/src/lib.rs",
                "pub mod items;\n\npub fn check() -> bool {\n    items::has_flag(None)\n}\n",
            ),
            ("crates/alpha/src/items.rs", HAS_FLAG),
            ("crates/beta/Cargo.toml", &beta),
            ("crates/beta/src/lib.rs", "pub mod items;\n"),
            ("crates/beta/src/items.rs", ""),
            ("crates/gamma/Cargo.toml", &gamma),
            (
                "crates/gamma/src/lib.rs",
                "pub fn other() -> i32 {\n    1\n}\n",
            ),
        ],
        "base",
    );
    fs::remove_file(repo.join("crates/alpha/src/items.rs")).unwrap();
    fs::write(
        repo.join("crates/alpha/Cargo.toml"),
        crate_manifest("alpha", alpha_dependencies),
    )
    .unwrap();
    fs::write(repo.join("crates/beta/src/items.rs"), HAS_FLAG).unwrap();
    fs::write(repo.join("crates/alpha/src/lib.rs"), alpha_lib).unwrap();
    fs::write(
        repo.join("crates/gamma/src/lib.rs"),
        format!(
            "pub fn other() -> i32 {{\n    1\n}}\n\npub fn {gamma_function}(s: Option<&str>) -> bool {{\n    s == Some(\"tasks\")\n}}\n"
        ),
    )
    .unwrap();
    for (path, source) in extra {
        let full = repo.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, source).unwrap();
    }
    detect_api_changes_from_worktree(repo)
}

const BETA_DEPENDENCY: &str = "beta = { path = \"../beta\" }\n";
const CHECK_THROUGH_BETA: &str = "pub fn check() -> bool {\n    beta::items::has_flag(None)\n}\n";

#[test]
fn rust_reference_sites_removal_keeps_conditional_dependency_roots() {
    for attribute in ["#![cfg(any())]", "#![cfg_attr(all(), cfg(any()))]"] {
        let root = format!("{attribute}\npub mod items;\n");
        let api = api_after_moving_to_other_crate(
            CHECK_THROUGH_BETA,
            BETA_DEPENDENCY,
            "has_flag",
            &[("crates/beta/src/lib.rs", &root)],
        );
        assert!(
            api.removed.iter().any(|symbol| symbol.name == "has_flag"),
            "{attribute}: {api:?}"
        );
        assert!(
            api.removed_dead
                .iter()
                .all(|symbol| symbol.name != "has_flag"),
            "{attribute}: {api:?}"
        );
    }
    let api = api_after_moving_to_other_crate(
        CHECK_THROUGH_BETA,
        BETA_DEPENDENCY,
        "has_flag",
        &[(
            "crates/beta/src/lib.rs",
            "#![cfg_attr(all(), allow(dead_code))]\npub mod items;\n",
        )],
    );
    assert!(
        api.removed_dead
            .iter()
            .any(|symbol| symbol.name == "has_flag"),
        "{api:?}"
    );
}

#[test]
fn rust_reference_sites_removal_keeps_nfc_equivalent_local_module_paths() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let alpha = crate_manifest("alpha", "K = { path = \"../beta\", package = \"beta\" }\n");
    let beta = crate_manifest("beta", "");
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"2\"\n",
            ),
            ("alpha/Cargo.toml", &alpha),
            (
                "alpha/src/lib.rs",
                "pub mod \u{212a} { pub mod items { pub fn run() {} } }\npub fn caller() { K::items::run(); }\n",
            ),
            ("beta/Cargo.toml", &beta),
            ("beta/src/lib.rs", "pub mod items { pub fn run() {} }\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("alpha/src/lib.rs"),
        "pub mod \u{212a} { pub mod items {} }\npub fn caller() { K::items::run(); }\n",
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "run"),
        "{api:?}"
    );
    assert!(
        api.removed_dead.iter().all(|symbol| symbol.name != "run"),
        "{api:?}"
    );
}

/// 移動先が 2 つで moved にならない削除でも、依存クレートの名前で始まるパスが依存先の残る
/// 関数に解決できれば、消した関数の残存参照に数えない (Issue #115)。
#[test]
fn rust_reference_sites_removal_resolves_paths_into_dependency_crates() {
    for (alpha_lib, dependencies, extra) in [
        // 再現: 依存のキー・改名した依存・`::` で始まる外部クレートのパス・workspace の依存
        (CHECK_THROUGH_BETA, BETA_DEPENDENCY, vec![]),
        (
            "pub fn check() -> bool {\n    shared::items::has_flag(None)\n}\n",
            "shared = { path = \"../beta\", package = \"beta\" }\n",
            vec![],
        ),
        (
            "pub fn check() -> bool {\n    ::beta::items::has_flag(None)\n}\n",
            BETA_DEPENDENCY,
            vec![],
        ),
        (
            CHECK_THROUGH_BETA,
            "beta = { workspace = true }\n",
            vec![(
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/gamma\"]\nresolver = \"2\"\n\n[workspace.dependencies]\nbeta = { path = \"crates/beta\" }\n",
            )],
        ),
    ] {
        let api = api_after_moving_to_other_crate(alpha_lib, dependencies, "has_flag", &extra);
        assert!(api.moved.is_empty(), "{alpha_lib}: {api:?}");
        assert!(
            api.removed.iter().all(|s| s.name != "has_flag"),
            "{alpha_lib}{dependencies}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().any(|s| s.name == "has_flag"),
            "{alpha_lib}{dependencies}: {api:?}"
        );
    }
    // 対照: 移動先が 1 つなら従来どおり moved
    let api =
        api_after_moving_to_other_crate(CHECK_THROUGH_BETA, BETA_DEPENDENCY, "has_other_flag", &[]);
    assert!(api.moved.iter().any(|s| s.name == "has_flag"), "{api:?}");
}

/// 依存クレートを指すと確かめられないパスと、消した関数を指し得るパスは api.rm に残す (Issue #115)。
#[test]
fn rust_reference_sites_removal_keeps_unproven_dependency_paths() {
    // 再公開はグロブにして、`has_flag` の語がパスの判定にだけ現れるようにする
    let local_beta = [(
        "crates/alpha/src/beta.rs",
        "pub mod items {\n    pub use crate::items::*;\n}\n",
    )];
    let beta_reexport = [
        (
            "crates/beta/src/lib.rs",
            "pub mod items;\npub mod reexport;\n",
        ),
        (
            "crates/beta/src/reexport.rs",
            "pub use alpha_old::items::*;\n",
        ),
    ];
    let cases: &[RemovalCase<'_>] = &[
        // 消した関数のモジュールを指すパスと import (cargo check は失敗する)
        (
            "pub fn check() -> bool {\n    crate::items::has_flag(None)\n}\n",
            BETA_DEPENDENCY,
            &[],
        ),
        (
            "use crate::items::has_flag;\n\npub fn check() -> bool {\n    has_flag(None)\n}\n",
            BETA_DEPENDENCY,
            &[],
        ),
        // 同じ名前のローカルの mod が消した関数を再公開し得る・use の別名・extern crate
        (
            "mod beta;\n\npub fn check() -> bool {\n    beta::items::has_flag(None)\n}\n",
            BETA_DEPENDENCY,
            &local_beta,
        ),
        (
            "use gamma as beta;\n\npub fn check() -> bool {\n    beta::has_flag(None)\n}\n",
            "beta = { path = \"../beta\" }\ngamma = { path = \"../gamma\" }\n",
            &[],
        ),
        (
            "extern crate self as beta;\n\npub fn check() -> bool {\n    beta::items::has_flag(None)\n}\n",
            BETA_DEPENDENCY,
            &[],
        ),
        (
            "macro_rules! bind { () => { extern crate self as beta; } }\nbind!();\npub mod caller;\n",
            BETA_DEPENDENCY,
            &[("crates/alpha/src/caller.rs", CHECK_THROUGH_BETA)],
        ),
        (
            "macro_rules! bind { () => { extern crate self as beta; } }\nbind!();\npub fn check() -> bool { ::beta::items::has_flag(None) }\n",
            BETA_DEPENDENCY,
            &[],
        ),
        (
            CHECK_THROUGH_BETA,
            "beta = { path = \"../beta\", package = \"different\" }\n",
            &[],
        ),
        (
            CHECK_THROUGH_BETA,
            "beta = { path = \"../beta\", optional = \"true\" }\n",
            &[],
        ),
        // グロブ・マクロの中・依存に無い名前・registry の依存・依存先の再公開・patch
        (
            "use std::collections::*;\n\npub fn check() -> bool {\n    beta::items::has_flag(None)\n}\n",
            BETA_DEPENDENCY,
            &[],
        ),
        (
            "macro_rules! call { ($e:expr) => { $e }; }\n\npub fn check() -> bool {\n    call!(beta::items::has_flag(None))\n}\n",
            BETA_DEPENDENCY,
            &[],
        ),
        (CHECK_THROUGH_BETA, "", &[]),
        (CHECK_THROUGH_BETA, "beta = \"0.1\"\n", &[]),
        (
            "pub fn check() -> bool {\n    beta::reexport::has_flag(None)\n}\n",
            BETA_DEPENDENCY,
            &beta_reexport,
        ),
        (
            CHECK_THROUGH_BETA,
            BETA_DEPENDENCY,
            &[(
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/gamma\"]\nresolver = \"2\"\n\n[patch.crates-io]\nbeta = { path = \"crates/alpha\" }\n",
            )],
        ),
    ];
    for (alpha_lib, dependencies, extra) in cases {
        let api = api_after_moving_to_other_crate(alpha_lib, dependencies, "has_flag", extra);
        assert!(
            api.removed.iter().any(|s| s.name == "has_flag"),
            "{alpha_lib}{dependencies}{extra:?}: {api:?}"
        );
        assert!(
            api.removed_dead.iter().all(|s| s.name != "has_flag"),
            "{alpha_lib}{dependencies}{extra:?}: {api:?}"
        );
    }
}
