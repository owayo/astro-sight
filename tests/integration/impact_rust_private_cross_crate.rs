//! 非公開の自由関数の変更で、それを名指せない別のクレートの同名の呼び出しを影響にしない (Issue #472)。

use super::support::*;

const A_OLD: &str = "mod child;\npub fn run(code: u32) -> Option<u32> {\n    translate(code, false)\n}\n\nfn translate(code: u32, test: bool) -> Option<u32> {\n    if test { None } else { Some(code) }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C\" fn exported() -> u32 {\n    0\n}\n";
/// 子のモジュールは変更した関数を呼ぶ (真の影響)。
const A_CHILD: &str = "use super::*;\npub fn again() -> Option<u32> {\n    translate(1, true)\n}\n";
/// `a` に依存しないクレート。glob の import と、自分の非公開の `translate` を持つ。
const B_LIB: &str = "use std::collections::*;\n\npub fn handle(event: &str) -> bool {\n    translate(event).is_some()\n}\n\nfn translate(event: &str) -> Option<usize> {\n    Some(event.len())\n}\n";

const CHILD_LINE: &str = "→ a/src/child.rs:3 [translate]";
const B_LINE: &str = "→ b/src/lib.rs:4 [translate]";

/// `a/src/lib.rs` の `translate` から引数を削った版。
fn shrink(a_old: &str) -> String {
    let a_new = a_old
        .replace("translate(code, false)", "translate(code)")
        .replace("code: u32, test: bool", "code: u32")
        .replace("if test { None } else { Some(code) }", "Some(code)");
    assert_ne!(a_new, a_old);
    a_new
}

/// 試すワークスペース。`members` の package の `Cargo.toml` は `dependencies` から作る。
struct Case<'a> {
    a_old: String,
    a_new: String,
    b_lib: &'a str,
    dependencies: Vec<(&'a str, &'a str)>,
    members: Vec<&'a str>,
    root_extra: &'a str,
    files: Vec<(&'a str, String)>,
    cargo_home: Option<&'a str>,
    /// `impact` に渡す環境変数。
    env: Vec<(&'a str, &'a str)>,
}

impl<'a> Case<'a> {
    fn new(a_old: &str) -> Self {
        Self {
            a_old: a_old.to_owned(),
            a_new: shrink(a_old),
            b_lib: B_LIB,
            dependencies: Vec::new(),
            members: vec!["a", "b"],
            root_extra: "",
            files: vec![("a/src/child.rs", A_CHILD.to_owned())],
            cargo_home: None,
            env: Vec::new(),
        }
    }

    fn depends(mut self, package: &'a str, dependencies: &'a str) -> Self {
        self.dependencies.push((package, dependencies));
        self
    }

    fn file(mut self, path: &'a str, source: impl Into<String>) -> Self {
        self.files.push((path, source.into()));
        self
    }

    fn changed_repo(&self) -> TestRepo {
        let repo = TestRepo::new();
        let members = self
            .members
            .iter()
            .map(|member| format!("\"{member}\""))
            .collect::<Vec<_>>()
            .join(", ");
        repo.write(
            "Cargo.toml",
            format!(
                "[workspace]\nmembers = [{members}]\nresolver = \"3\"\n{}",
                self.root_extra
            ),
        );
        for name in &self.members {
            // glob の member (`targe?/level/*`) は、合う package を `files` で置く。
            if name.contains(['*', '?', '[']) {
                continue;
            }
            let dependencies = self
                .dependencies
                .iter()
                .filter(|(package, _)| package == name)
                .map(|(_, dependencies)| *dependencies)
                .collect::<String>()
                .replace(
                    "{root}",
                    &repo.root().display().to_string().replace('\\', "/"),
                )
                .replace(
                    "{root_name}",
                    &repo.root().file_name().unwrap().to_string_lossy(),
                );
            repo.create_dir_all(format!("{name}/src"));
            repo.write(
                format!("{name}/Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}"),
            );
        }
        repo.write("a/src/lib.rs", &self.a_old);
        repo.write("b/src/lib.rs", self.b_lib);
        for (path, source) in &self.files {
            if let Some(parent) = std::path::Path::new(path).parent() {
                repo.create_dir_all(parent);
            }
            repo.write(path, source);
        }
        repo.init_git();
        repo.commit_all("initial");
        repo.write("a/src/lib.rs", &self.a_new);
        repo
    }

    /// `impact --dir . --git` の終了コードと未解決の影響の行を返す。
    fn run(self) -> (i32, Vec<String>) {
        let repo = self.changed_repo();
        let mut command = cargo_bin();
        command
            .current_dir(repo.root())
            .args(["impact", "--dir", ".", "--git"]);
        if let Some(home) = self.cargo_home {
            repo.create_dir_all(home);
            command.env("CARGO_HOME", repo.root().join(home));
        }
        command.envs(self.env.iter().copied());
        let output = command.output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        let lines = stderr
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('→'))
            .map(str::to_owned)
            .collect();
        (output.status.code().unwrap(), lines)
    }
}

#[test]
fn rust_extern_signature_change_reaches_the_dependent_crate() {
    let mut case = Case::new(A_OLD).depends("b", "a = { path = \"../a\" }\n");
    case.a_old.push_str("\npub mod api {\n    unsafe extern \"C\" {\n        pub fn translate(x: u8);\n        fn local(x: u8);\n    }\n}\n");
    case.a_new = case
        .a_old
        .replace("fn translate(x: u8);", "fn translate();")
        .replace("fn local(x: u8);", "fn local();");
    case.b_lib = "pub fn run() {\n    unsafe { a::api::translate(1); }\n}\n";
    let repo = case.changed_repo();
    let review = repo.run_json("review", &["--git"]);
    let modified = review["api_changes"]["modified"].as_array().unwrap();
    assert_eq!(modified.len(), 1, "{modified:?}");
    assert_eq!(modified[0]["name"], "translate", "{modified:?}");
    let output = cargo_bin()
        .current_dir(repo.root())
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("→ b/src/lib.rs:2 [translate]"), "{stderr}");

    for extra in [None, Some("--staged")] {
        if extra.is_some() {
            repo.stage_all();
            repo.write("a/src/lib.rs", &case.a_old);
        }
        let output = cargo_bin()
            .current_dir(repo.root())
            .args(["review", "--dir", ".", "--git", "--hook"])
            .args(extra)
            .output()
            .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        let hook: serde_json::Value = serde_json::from_str(&stderr).unwrap();
        let modified = hook["api"]["mod"].as_array().unwrap();
        assert_eq!(modified.len(), 1, "{hook}");
        assert_eq!(modified[0]["n"], "translate", "{hook}");
        assert!(
            hook["impacts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|group| group["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["p"] == "b/src/lib.rs"
                        && r["ln"] == 1
                        && r["s"] == serde_json::json!(["translate"]))),
            "{hook}"
        );
    }
}

#[test]
fn rust_extern_formatting_preserves_the_caller_with_crlf() {
    let mut case = Case::new(A_OLD).depends("b", "a = { path = \"../a\" }\n");
    case.a_old = "unsafe extern \"C\" {\r\n    pub fn render(x: u8);\r\n}\r\n".to_owned();
    case.b_lib = "pub const RENDER: unsafe extern \"C\" fn(u8) = a::render;\n";
    let wrapped = case
        .a_old
        .replace("render(x: u8)", "render(\r\n        x: u8,\r\n    )");
    for (source, blocking) in [
        (wrapped.clone(), false),
        (wrapped.replace("x: u8", "x: u16"), true),
        (wrapped.replace("\"C\"", "\"C-unwind\""), true),
    ] {
        case.a_new = source;
        let repo = case.changed_repo();
        for command in ["impact", "review"] {
            let result = cargo_bin()
                .current_dir(repo.root())
                .args([command, "--dir", ".", "--git", "--hook"])
                .output()
                .unwrap();
            assert_eq!(
                result.status.code(),
                Some(i32::from(blocking)),
                "{command}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}

#[test]
fn private_inherent_calls_resolve_the_qualified_owner_in_a_workspace() {
    let mut case = Case::new(A_OLD);
    case.a_old =
        "mod child;\npub struct S;\nimpl S {\n    fn translate(_: u8) {}\n}\nfn r#translate() {}\n"
            .into();
    case.a_new = case.a_old.replace("_: u8", "");
    case.files = vec![(
        "a/src/child.rs",
        r#"use super::S as Owner;
struct S;
impl S { fn translate(_: u8) {} }
fn run() {
    super::S::translate(1);
    crate::S::translate(1);
    Owner::translate(1);
    S::translate(1);
}
mod inner {
    fn run() {
        super::super::S::translate(1);
        super::S::translate(1);
    }
}
fn rebound() {
    type Owner = S;
    r#Owner::translate(1);
}
"#
        .into(),
    )];
    let repo = case.changed_repo();
    for staged in [false, true] {
        let mut args = vec!["--git"];
        if staged {
            repo.stage_all();
            repo.write("a/src/lib.rs", &case.a_old);
            args.push("--staged");
        }
        let review = repo.run_json("review", &args);
        let changes = &review["impact"]["changes"][0];
        let callers = changes["impacted_callers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let reported = callers
            .iter()
            .filter(|c| {
                c["symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s == "translate")
            })
            .map(|c| (c["path"].as_str().unwrap(), c["line"].as_u64().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            reported,
            [
                ("a/src/child.rs", 4),
                ("a/src/child.rs", 5),
                ("a/src/child.rs", 6),
                ("a/src/child.rs", 11)
            ],
            "{changes}"
        );
        let result = cargo_bin()
            .current_dir(repo.root())
            .args(["review", "--dir", ".", "--hook"])
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn private_inherent_call_keeps_a_conditional_method_uncertain() {
    for implementation in [
        "impl S {\n    #[cfg(any())]\n    fn translate(_: u8) {}\n}",
        "impl S {\n    // disabled impl\n    #![cfg(any())]\n    fn translate(_: u8) {}\n}",
        "impl S {\n    fn translate(_: u8) {\n        /* disabled method */\n        #![cfg(any())]\n    }\n}",
    ] {
        let mut case = Case::new(A_OLD);
        case.files = vec![(
            "a/src/child.rs",
            "trait F { fn translate(_: u8) {} }\nimpl F for super::S {}\nfn run() { super::S::translate(1); }\n".into(),
        )];
        case.a_old = format!("mod child;\npub struct S;\n{implementation}\n");
        case.a_new = case.a_old.replace("_: u8", "");
        assert_eq!(case.run(), (0, vec![]), "{implementation}");
    }
}

#[test]
fn impact_rust_private_function_excludes_crates_that_cannot_name_it() {
    // Issue の最小の再現 (別の項目の `#[unsafe(no_mangle)]` と、`b` の glob の import)。
    assert_eq!(Case::new(A_OLD).run(), (1, vec![CHILD_LINE.to_owned()]));
    // 名前を束縛し直さない組み込みの属性は、旧新で同じなら付いていてもよい。
    for attribute in ["#[inline]", "#[cfg(all())]"] {
        let a_old = A_OLD.replace("fn translate(", &format!("{attribute}\nfn translate("));
        assert_eq!(
            Case::new(&a_old).run(),
            (1, vec![CHILD_LINE.to_owned()]),
            "{attribute}"
        );
    }
    // 木の中に `macro_rules!` があれば、関数の本体の中の式のマクロ (`pass!(translate(..))`) でも
    // 除かない (字句を組み立てて取り込む経路を、マクロごとに確かめ切らない)。
    let a_old = format!(
        "macro_rules! pass {{\n    ($e:expr) => {{ $e }};\n}}\n{}",
        A_OLD.replace(
            "    translate(code, false)\n",
            "    pass!(translate(code, false))\n"
        )
    );
    assert_eq!(
        Case::new(&a_old).run(),
        (
            1,
            vec![
                CHILD_LINE.to_owned(),
                "→ b/src/lib.rs:4 [translate]".to_owned()
            ]
        )
    );
    // registry の依存があっても、lock に `b` の写しが無ければ除く (Kanade の `windows` の形)。
    let lock = "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\ndependencies = [\n \"serde\",\n]\n\n[[package]]\nname = \"b\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    assert_eq!(
        Case::new(A_OLD)
            .depends("a", "serde = \"1\"\n")
            .file("Cargo.lock", lock)
            .run(),
        (1, vec![CHILD_LINE.to_owned()])
    );
    // `a` から届かない別の member (`d`) だけが、公開した `b` の写しに依存する。
    let lock = format!(
        "{lock}\n[[package]]\nname = \"b\"\nversion = \"0.2.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"d\"\nversion = \"0.1.0\"\ndependencies = [\n \"b 0.2.0 (registry+https://github.com/rust-lang/crates.io-index)\",\n]\n"
    );
    let mut case = Case::new(A_OLD)
        .depends("a", "serde = \"1\"\n")
        .depends("d", "b = \"0.2\"\n")
        .file("d/src/lib.rs", "")
        .file("Cargo.lock", lock);
    case.members.push("d");
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    // proc-macro の `quote!` の `#x` (木の外のマクロ) は、属性を組み立てる裸の `#` として数えない。
    let mut case = Case::new(A_OLD)
        .depends("d", "quote = \"1\"\n")
        .file(
            "d/src/lib.rs",
            "use quote::quote;\npub fn generate(name: &str) {\n    let body = 1u8;\n    let _ = quote::quote! { fn #name() { #body } };\n    let _ = quote! { #name };\n}\n",
        );
    case.members.push("d");
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    // 木の中に `macro_rules!` の定義があれば (変更元にも参照元にも届かない別の member の中でも、
    // 何もしないマクロでも)、字句を組み立てて取り込み得るので除かない。
    let mut case = Case::new(A_OLD).file(
        "d/src/lib.rs",
        "macro_rules! trace {\n    () => {};\n}\npub fn tick() {\n    trace!();\n}\n",
    );
    case.members.push("d");
    assert_eq!(
        case.run(),
        (1, vec![CHILD_LINE.to_owned(), B_LINE.to_owned()])
    );
    // 別 member の `#[path]` で読む `.rs` でないファイルの中だけにある定義も数える。対照: 定義の
    // 無い `.inc` は除く。
    for (inc, expected) in [
        (
            "macro_rules! trace {\n    () => {};\n}\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
        ("pub fn tick() {}\n", vec![CHILD_LINE.to_owned()]),
    ] {
        let mut case = Case::new(A_OLD)
            .file("d/src/lib.rs", "#[path = \"defs.inc\"]\nmod defs;\n")
            .file("d/src/defs.inc", inc);
        case.members.push("d");
        assert_eq!(case.run(), (1, expected), "{inc}");
    }
    // token tree の中だけの `macro_rules` (定義ではない) も、マクロがマクロを作り得るので数える。
    let mut case = Case::new(A_OLD).file(
        "d/src/lib.rs",
        "pub fn names() -> &'static str {\n    stringify!(macro_rules)\n}\n",
    );
    case.members.push("d");
    assert_eq!(
        case.run(),
        (1, vec![CHILD_LINE.to_owned(), B_LINE.to_owned()])
    );
    // 先頭行の `#! [..]` は rustc では内側の属性で、同じ行の定義・`#[path]` は同梱の文法では読めない
    // (行全体が shebang になる)。空白を詰めた `#![..]` と、`[` の無い普通の shebang が対照。
    // 普通の内側の属性だけのファイル (Kanade の `#![cfg(target_os = "macos")]` の形) は除く。
    for (extra, expected) in [
        (
            "#! [allow(dead_code)] macro_rules! mount { () => { include!(\"../../b/src/lib.rs\"); }; }\nmount!();\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
        (
            "#! [allow(dead_code)] #[path = \"../../b/src/lib.rs\"] mod copied;\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
        (
            "#![allow(dead_code)] macro_rules! mount { () => {}; }\nmount!();\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
        (
            "#!/usr/bin/env run-cargo-script\npub fn tick() {}\n",
            vec![CHILD_LINE.to_owned()],
        ),
        (
            "#![cfg(all())]\n#![allow(dead_code)]\npub fn tick() {}\n",
            vec![CHILD_LINE.to_owned()],
        ),
    ] {
        let case = Case::new(&format!("mod extra;\n{A_OLD}")).file("a/src/extra.rs", extra);
        assert_eq!(case.run(), (1, expected), "{extra}");
    }
    // root の外を指す path の依存は、走査が読まない package (木のファイルを取り込み得る) を依存に
    // する。対照: root の中の path の依存は除く。
    for (dependency, expected) in [
        (
            "probe = { path = \"../../outside-probe\" }\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
        (
            "probe = { path = \"../probe\" }\n",
            vec![CHILD_LINE.to_owned()],
        ),
    ] {
        let case = Case::new(A_OLD)
            .depends("a", dependency)
            .file(
                "probe/Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .file("probe/src/lib.rs", "");
        assert_eq!(case.run(), (1, expected), "{dependency}");
    }
    // 先頭のコメントの後の `#! [..]` (同梱の文法は shebang にする) と、`#[path]` で読む `.rs` でない
    // ファイルの先頭の `#! [..]`。root の外の workspace に属す package (継承する依存の先を読めない)。
    // nightly の `macro` 項目 (`#![feature(decl_macro)]`)。
    let hidden = "#! [allow(dead_code)] #[path = \"../../b/src/lib.rs\"] mod copied;\n";
    let cases = [
        Case::new(&format!("mod extra;\n{A_OLD}"))
            .file("a/src/extra.rs", format!("// license\n{hidden}")),
        // `path` の綴りの無いファイルの、先頭のコメントの後の `#! [..]` が隠す `mod lib;`
        // (`#[path]` で読む `b/src/bridge.rs` の隣の `b/src/lib.rs`)。
        Case::new(&format!(
            "#[path = \"../../b/src/bridge.rs\"]\nmod bridge;\n{A_OLD}"
        ))
        .file("b/src/bridge.rs", "// license\n#! [allow(dead_code)] mod lib;\n"),
        // 先頭の `#!` の後のブロックコメントが次の行で閉じ、`[` が続く (rustc では内側の属性)。
        Case::new(&format!("mod extra;\n{A_OLD}")).file(
            "a/src/extra.rs",
            "#! /*\n// */ [allow(dead_code)] #[path = \"../../b/src/lib.rs\"] mod copied;\n",
        ),
        // 古い `[project]` の表の、root の直下の `target` の下を指す `workspace`。
        Case::new(A_OLD)
            .file(
                "e/Cargo.toml",
                "[project]\nname = \"e\"\nversion = \"0.1.0\"\nedition = \"2021\"\nworkspace = \"../target/debug\"\n",
            )
            .file("e/src/lib.rs", ""),
        Case::new(A_OLD)
            .file(
                "e/Cargo.toml",
                "[package]\nname = \"e\"\nversion = \"0.1.0\"\nedition = \"2024\"\nworkspace = \"../target/debug\"\n",
            )
            .file("e/src/lib.rs", ""),
        Case::new(&format!("#[path = \"extra.inc\"]\nmod extra;\n{A_OLD}"))
            .file("a/src/extra.inc", hidden),
        Case::new(A_OLD)
            .file(
                "e/Cargo.toml",
                "[package]\nname = \"e\"\nversion = \"0.1.0\"\nedition = \"2024\"\nworkspace = \"../../outer\"\n",
            )
            .file("e/src/lib.rs", ""),
    ];
    for case in cases {
        assert_eq!(
            case.run(),
            (1, vec![CHILD_LINE.to_owned(), B_LINE.to_owned()])
        );
    }
    // nightly の `macro` 項目は、機能の綴りが `.rs` に無くても (`-Zcrate-attr`) 数える。
    for lib in [
        "#![feature(decl_macro)]\npub macro mount() { mod calls; }\n",
        "pub macro mount() { mod calls; }\n",
    ] {
        let mut case = Case::new(A_OLD).file("d/src/lib.rs", lib);
        case.members.push("d");
        assert_eq!(
            case.run(),
            (1, vec![CHILD_LINE.to_owned(), B_LINE.to_owned()]),
            "{lib}"
        );
    }
    // 木の外のマクロの引数の中の `macro` 項目 (`cfg_if!` が定義を出す)。
    let mut case = Case::new(A_OLD)
        .depends("d", "cfg-if = \"1\"\n")
        .file(
            "d/src/lib.rs",
            "cfg_if::cfg_if! {\n    if #[cfg(all())] {\n        pub macro mount() { mod calls; }\n    }\n}\n",
        );
    case.members.push("d");
    assert_eq!(
        case.run(),
        (1, vec![CHILD_LINE.to_owned(), B_LINE.to_owned()])
    );
    // 古い `[project]` の表は、`workspace` が無くても配置を決めない (依存のたどり方は `[project]` の
    // `workspace` を読まない)。
    let mut case = Case::new(A_OLD)
        .file(
            "e/Cargo.toml",
            "[project]\nname = \"e\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .file("e/src/lib.rs", "");
    case.members.push("e");
    assert_eq!(
        case.run(),
        (1, vec![CHILD_LINE.to_owned(), B_LINE.to_owned()])
    );
    // `.rs` を読む `include_str!` は、`#[doc = ..]` で別の package のファイルを doctest の本体にできる。
    // 対照: 文書 (`README.md`) を読む `include_str!` なら除く。
    for (guide, expected) in [
        (
            "#[doc = concat!(\"```rust\\n\", include_str!(\"../../b/src/lib.rs\"), \"\\n```\")]\npub struct Guide;\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
        (
            "#[doc = include_str!(\"../README.md\")]\npub struct Guide;\n",
            vec![CHILD_LINE.to_owned()],
        ),
        (
            "#[doc = include_str!(concat!(\"../../b/src/lib.r\", 's'))]\npub struct Guide;\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
    ] {
        let case = Case::new(&format!("mod guide;\n{A_OLD}"))
            .file("a/src/guide.rs", guide)
            .file("a/README.md", "# a\n");
        assert_eq!(case.run(), (1, expected), "{guide}");
    }
    // root の `exclude` は Cargo と同じく manifest のパスと照合する (`e/Cargo.toml` も `e` を除き、除いた
    // package は root の workspace に属さないので決めない)。対照: 除かなければ root の member。
    for (exclude, expected) in [
        (
            "exclude = [\"e/Cargo.toml\"]\n",
            vec![CHILD_LINE.to_owned(), B_LINE.to_owned()],
        ),
        ("", vec![CHILD_LINE.to_owned()]),
    ] {
        let mut case = Case::new(A_OLD)
            .depends("b", "e = { path = \"../e\" }\n")
            .file(
                "e/Cargo.toml",
                "[package]\nname = \"e\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .file("e/src/lib.rs", "");
        case.root_extra = exclude;
        assert_eq!(case.run(), (1, expected), "{exclude}");
    }
    // 対照: コメントの `decl_macro`・`macro` の語だけなら除く。
    let mut case = Case::new(A_OLD).file(
        "d/src/lib.rs",
        "// decl_macro and macro words in a comment\npub fn tick() {}\n",
    );
    case.members.push("d");
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    // 先頭の要素が glob の member (`*/plugin`) でも、`target` の下に合う package が無ければ除く。
    let mut case = Case::new(A_OLD)
        .file(
            "apps/plugin/Cargo.toml",
            "[package]\nname = \"plugin\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file("apps/plugin/src/lib.rs", "");
    case.members.push("*/plugin");
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
}

/// `b/src/lib.rs` で `needle` を含む最後の行の、未解決の影響の行。
fn b_line(b_lib: &str, needle: &str) -> String {
    let lines: Vec<&str> = b_lib.lines().collect();
    let line = lines
        .iter()
        .rposition(|line| line.contains(needle))
        .unwrap()
        + 1;
    format!("→ b/src/lib.rs:{line} [translate]")
}

/// マクロの引数の中の参照は、展開の規則を確かめたマクロ (registry の objc2 0.6 の `define_class!` の
/// `impl` のメソッドの本体。Kanade の形) だけを除く。木の中に定義のあるマクロ・ほかのマクロの引数・
/// 属性マクロを付けた項目は、別のクレートで展開されるコードへ運ばれ得るので残す。
#[test]
fn impact_rust_private_function_resolves_macro_arguments_only_for_known_expansions() {
    const DEFINE: &str = "define_class!(\n    #[unsafe(super(NSObject))]\n    pub struct Handler;\n    impl Handler {\n        #[unsafe(method(key:))]\n        fn key(&self, event: &str) -> bool {\n            guarded(|| { translate(event).is_some() })\n        }\n    }\n);\n";
    let objc2 = "objc2 = \"0.6.5\"\n";
    let run = |b_lib: &str, dependencies: &'static str| {
        let mut case = Case::new(A_OLD).depends("b", dependencies);
        case.b_lib = b_lib;
        case.run()
    };
    // 除く: objc2 から import した裸の `define_class!`。対象の OS だけの依存と、workspace から
    // 受け継ぐ edition (Kanade の形) も。
    let imported = format!("{B_LIB}\nuse objc2::{{define_class, msg_send}};\n{DEFINE}");
    assert_eq!(run(&imported, objc2), (1, vec![CHILD_LINE.to_owned()]));
    // 無関係なフィールドの `include` を標準の書式のマクロへ渡すだけなら除く。
    let noisy = format!(
        "{imported}\npub struct Opts {{\n    pub include: bool,\n}}\npub fn show(opts: &Opts) {{\n    println!(\"{{}}\", opts.include);\n}}\n"
    );
    assert_eq!(run(&noisy, objc2), (1, vec![CHILD_LINE.to_owned()]));
    // 木の中に `macro_rules!` の定義があれば (何もしないマクロでも) 除かない。
    let with_macro = format!(
        "{imported}\nmacro_rules! trace {{\n    () => {{}};\n}}\npub fn tick() {{\n    trace!();\n}}\n"
    );
    let (_, lines) = run(&with_macro, objc2);
    assert!(
        lines.contains(&b_line(&with_macro, "translate(event)")),
        "{lines:?}"
    );
    let mut case = Case::new(A_OLD).file(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition.workspace = true\n\n[target.'cfg(target_os = \"macos\")'.dependencies]\nobjc2 = { version = \"0.6.5\", features = [\"exception\"] }\n",
    );
    case.root_extra = "\n[workspace.package]\nedition = \"2024\"\n";
    case.b_lib = &imported;
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    // 対照: 組み込みの属性だけの関数の中は除く。
    let inline = format!(
        "{B_LIB}\n#[inline]\npub fn run(event: &str) -> bool {{\n    translate(event).is_some()\n}}\n"
    );
    assert_eq!(run(&inline, ""), (1, vec![CHILD_LINE.to_owned()]));
    // 残す。
    let in_tree = format!(
        "{B_LIB}\nmacro_rules! define_class {{\n    ($($body:tt)*) => {{ $($body)* }};\n}}\n{DEFINE}"
    );
    let aliased = format!(
        "{B_LIB}\nuse objc2::define_class as declare;\n{}",
        DEFINE.replace("define_class!", "declare!")
    );
    // `objc2::define_class!` の `objc2` は、モジュールの束縛 (`use crate::shim as objc2;`) で付け替え
    // られる。
    let scoped = format!("{B_LIB}\nobjc2::{DEFINE}");
    let in_body = |from: &str, to: &str| {
        assert!(DEFINE.contains(from));
        format!(
            "{B_LIB}\nuse objc2::define_class;\n{}",
            DEFINE.replace(from, to)
        )
    };
    let guarded = "guarded(|| { translate(event).is_some() })";
    let nested = in_body(guarded, "guarded(|| { pass!(translate(event)).is_some() })");
    // 引数の前のコメントは、マクロの引数であることを変えない。
    let commented = in_body(
        guarded,
        "guarded(|| { pass! /* note */ (translate(event)).is_some() })",
    );
    // 本体の中の、属性を付けた項目は属性マクロへ渡り得る。
    let inner_item = in_body(
        guarded,
        "#[emit]\n            fn template() {\n                translate(\"x\");\n            }\n            true",
    );
    // メソッドと `impl` の、objc2 の印でない属性は生成するメソッド・`impl` に付く。
    let method_attribute = in_body(
        "#[unsafe(method(key:))]",
        "#[unsafe(method(key:))]\n        #[emit]",
    );
    let impl_attribute = in_body("    impl Handler {", "    #[emit]\n    impl Handler {");
    // 戻り値の型の位置のマクロの引数 (`-> ret! { .. }`) は、メソッドの本体でない。
    let return_macro = in_body(
        "-> bool {\n            guarded(|| { translate(event).is_some() })\n        }",
        "-> ret! { translate(event) } {\n            None\n        }",
    );
    // `#[test]` は同じ名前の属性マクロの import で付け替えられる。
    let test_alias = format!(
        "{B_LIB}\nuse macro_magic::export_tokens_no_emit as test;\n#[test]\npub const TEMPLATE: fn() = || {{\n    translate(\"x\");\n}};\n"
    );
    let other_macro = format!(
        "{B_LIB}\ncfg_if::cfg_if! {{\n    if #[cfg(all())] {{\n        pub fn run(event: &str) -> bool {{\n            translate(event).is_some()\n        }}\n    }}\n}}\n"
    );
    let attribute = format!(
        "{B_LIB}\n#[macro_magic::export_tokens]\npub fn run(event: &str) -> bool {{\n    translate(event).is_some()\n}}\n"
    );
    let cfg_attr = format!(
        "{B_LIB}\n#[cfg_attr(all(), macro_magic::export_tokens)]\npub fn run(event: &str) -> bool {{\n    translate(event).is_some()\n}}\n"
    );
    let body =
        format!("{B_LIB}\nmacro_rules! call_translate {{\n    () => {{ translate(1) }};\n}}\n");
    let imported_line = b_line(&imported, "guarded(");
    for (b_lib, dependencies, needle) in [
        (in_tree.as_str(), objc2, "guarded("),
        (aliased.as_str(), objc2, "guarded("),
        (scoped.as_str(), objc2, "guarded("),
        (nested.as_str(), objc2, "guarded("),
        (commented.as_str(), objc2, "guarded("),
        (inner_item.as_str(), objc2, "translate(\"x\")"),
        (method_attribute.as_str(), objc2, "guarded("),
        (impl_attribute.as_str(), objc2, "guarded("),
        (return_macro.as_str(), objc2, "ret!"),
        (imported.as_str(), "objc2 = \"0.5.2\"\n", "guarded("),
        (
            imported.as_str(),
            "objc2 = { version = \"0.6.5\", package = \"objc2-fork\" }\n",
            "guarded(",
        ),
        (test_alias.as_str(), "", "translate(\"x\")"),
        (other_macro.as_str(), "", "translate("),
        (attribute.as_str(), "", "translate("),
        (cfg_attr.as_str(), "", "translate("),
        (body.as_str(), "", "translate("),
    ] {
        // 足した呼び出しは、`needle` が最後に現れる行。
        let line = b_line(b_lib, needle);
        let (_, lines) = run(b_lib, dependencies);
        assert!(lines.contains(&line), "{b_lib}\n{lines:?}");
    }
    // 属性を付けた `use` (無効になり得る) と、中のモジュールの呼び出し (ファイル直下の `use` が
    // 届かない) は残す。
    for (b_lib, needle) in [
        (
            format!("{B_LIB}\n#[cfg(target_os = \"macos\")]\nuse objc2::define_class;\n{DEFINE}"),
            "guarded(",
        ),
        (
            format!(
                "{B_LIB}\nuse objc2::define_class;\nmod inner {{\n    use super::translate;\n{DEFINE}}}\n"
            ),
            "guarded(",
        ),
    ] {
        let line = b_line(&b_lib, needle);
        let (_, lines) = run(&b_lib, objc2);
        assert!(lines.contains(&line), "{b_lib}\n{lines:?}");
    }
    // 木のどこかで `define_class`・`objc2` を束縛し直す (別のファイルの `pub use .. as define_class;`・
    // `use .. as objc2;`・ほかのマクロの引数の `objc2::tag`・`#[macro_use]`)、edition 2015
    // (`use objc2::..` が crate の根からの相対になる。target ごとの `edition` も)、`objc2` が build の
    // 依存だけ (ライブラリに渡らない) は残す。
    let line = b_line(&imported, "guarded(");
    for (path, source) in [
        (
            "b/src/shim.rs",
            "pub use crate::export_body as define_class;\n",
        ),
        ("tools/src/lib.rs", "use crate::shim as objc2;\n"),
        ("tools/src/bind.rs", "bind!(objc2::tag);\n"),
        (
            "tools/src/macros.rs",
            "#[macro_use]\n#[path = \"../support/defs.inc\"]\nmod defs;\n",
        ),
        (
            "tools/src/prelude.rs",
            "#[no_implicit_prelude]\nmod native;\n",
        ),
        // マクロの本体の中の属性 (`off!(mod native;)` が `#[no_implicit_prelude] mod native;` になる)。
        (
            "tools/src/off.rs",
            "macro_rules! off {\n    ($i:item) => { #[no_implicit_prelude] $i };\n}\n",
        ),
        // 走査が読まないソース: `.rs` でないファイル・生成物の取り込み、木の中の proc-macro のクレート。
        (
            "tools/src/macros.rs",
            "#[path = \"../support/defs.inc\"]\nmod defs;\n",
        ),
        (
            "tools/src/generated.rs",
            "include!(concat!(env!(\"OUT_DIR\"), \"/bindings.rs\"));\n",
        ),
        (
            "helpers/Cargo.toml",
            "[package]\nname = \"helpers\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n",
        ),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nobjc2 = { version = \"0.6.5\", optional = true }\n",
        ),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n\n[dependencies]\nobjc2 = \"0.6.5\"\n",
        ),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nedition = \"2015\"\n\n[dependencies]\nobjc2 = \"0.6.5\"\n",
        ),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[build-dependencies]\nobjc2 = \"0.6.5\"\n",
        ),
    ] {
        let dependencies = if source.contains("[build-dependencies]") {
            ""
        } else {
            objc2
        };
        let mut case = Case::new(A_OLD)
            .depends("b", dependencies)
            .file(path, source)
            .file(
                "tools/support/defs.inc",
                "macro_rules! helper {\n    () => {};\n}\n",
            );
        case.b_lib = &imported;
        let (_, lines) = case.run();
        assert!(lines.contains(&line), "{path}\n{source}\n{lines:?}");
    }
    // 対照: 綴りがコメントと属性の値の文字列にあるだけ、木の中の `.rs` を字面のパスで読む
    // (`include!`・`#[path]`) なら除く。
    for source in [
        "// macro_use and no_implicit_prelude are not used\n",
        "#[doc = \"no_implicit_prelude\"]\nfn documented() {}\n",
        "pub mod generated {\n    include!(\"generated.rs\");\n}\n#[path = \"../support/extra.rs\"]\nmod extra;\n",
    ] {
        let mut case = Case::new(A_OLD)
            .depends("b", objc2)
            .file("tools/src/macros.rs", source)
            .file(
                "tools/support/defs.inc",
                "macro_rules! helper {\n    () => {};\n}\n",
            )
            .file("tools/src/generated.rs", "pub fn generated() {}\n")
            .file("tools/support/extra.rs", "pub fn extra() {}\n");
        case.b_lib = &imported;
        assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]), "{source}");
    }
    // 対照: `optional = false` は常に有効な通常の依存。`macro_use` がコメントにあるだけの import。
    let mut case = Case::new(A_OLD).file(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nobjc2 = { version = \"0.6.5\", optional = false }\n",
    );
    case.b_lib = &imported;
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    let commented_import = format!("// macro_use は不要\n{imported}");
    let mut case = Case::new(A_OLD).depends("b", objc2);
    case.b_lib = &commented_import;
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    assert_ne!(imported_line, CHILD_LINE);
}

/// 公開マクロの本体の属性の文字列 (`#[serde(default = "crate::translate")]`) の参照は、マクロを
/// 展開したクレート (`a`) で名前を解決するので除かない。
#[test]
fn impact_rust_private_function_keeps_attribute_strings_in_exported_macros() {
    let b_lib = "#[macro_export]\nmacro_rules! fields {\n    () => {\n        #[derive(serde::Deserialize)]\n        pub struct Fields {\n            #[serde(default = \"crate::translate\")]\n            pub value: u32,\n        }\n    };\n}\n";
    // `a` の `translate` は serde の既定値の関数 (`fn() -> u32`) で、引数を足すと展開した属性が壊れる。
    let a_old = "mod child;\nfn translate() -> u32 {\n    0\n}\n";
    let mut case = Case::new(A_OLD)
        .depends(
            "a",
            "b = { path = \"../b\" }\nserde = { version = \"1\", features = [\"derive\"] }\n",
        )
        .file("a/src/child.rs", "b::fields!();\n");
    case.a_old = a_old.to_owned();
    case.a_new = a_old.replace("fn translate() -> u32", "fn translate(_: u8) -> u32");
    case.b_lib = b_lib;
    let (_, lines) = case.run();
    assert!(
        lines.contains(&"→ b/src/lib.rs:6 [translate]".to_owned()),
        "{lines:?}"
    );
}

/// `b` の `#[macro_export]` の本体 (13 行目) の呼び出しは、展開した位置で名前を解決する。
const B_MACRO: &str = "use std::collections::*;\n\npub fn handle(event: &str) -> bool {\n    translate(event).is_some()\n}\n\nfn translate(event: &str) -> Option<usize> {\n    Some(event.len())\n}\n\n#[macro_export]\nmacro_rules! call_translate {\n    () => { translate(1, false) };\n}\n";
const B_MACRO_LINE: &str = "→ b/src/lib.rs:13 [translate]";
#[test]
fn impact_rust_private_function_keeps_calls_that_may_reach_it() {
    // `a` が `b` に依存し、`b` のマクロを `a` の中で展開する。
    let expands =
        format!("{A_OLD}\npub fn via_macro() -> Option<u32> {{\n    b::call_translate!()\n}}\n");
    let mut case = Case::new(&expands).depends("a", "b = { path = \"../b\" }\n");
    case.b_lib = B_MACRO;
    let (code, lines) = case.run();
    assert_eq!(code, 1);
    assert!(lines.contains(&B_MACRO_LINE.to_owned()), "{lines:?}");
    for a_old in [
        // 記号の名前で、依存の無いクレートからも `extern` で呼べる。
        A_OLD.replace(
            "fn translate(code: u32, test: bool)",
            "#[unsafe(no_mangle)]\nextern \"C\" fn translate(code: u32, test: bool)",
        ),
        // 属性マクロは可視性を変え得る。
        A_OLD.replace("fn translate(", "#[custom::expose]\nfn translate("),
        A_OLD.replace(
            "fn translate(",
            "#[cfg_attr(all(), custom::expose)]\nfn translate(",
        ),
        // `b` のファイルが `a` のモジュールにも入る。
        format!("#[path = \"../../b/src/lib.rs\"]\nmod copy;\n{A_OLD}"),
    ] {
        let (_, lines) = Case::new(&a_old).run();
        assert!(lines.contains(&B_LINE.to_owned()), "{a_old}\n{lines:?}");
    }
    // 旧い版で公開・別の属性だった関数を、新しい版で非公開・属性なしにする。
    for old in [
        "pub fn translate(",
        "#[unsafe(export_name = \"t\")]\nfn translate(",
    ] {
        let mut case = Case::new(&A_OLD.replace("fn translate(", old));
        case.a_new = shrink(A_OLD);
        let (_, lines) = case.run();
        assert!(lines.contains(&B_LINE.to_owned()), "{old}\n{lines:?}");
    }
    // `b` が `a` に依存すると、`a` の公開の別名や `#[macro_export]` のマクロを経て、`b` の
    // `translate(..)` が変更した関数に着き得る。
    let (_, lines) = Case::new(A_OLD)
        .depends("b", "a = { path = \"../a\" }\n")
        .run();
    assert!(lines.contains(&B_LINE.to_owned()), "{lines:?}");
    for (macros, a_old, a_new, b_lib, line) in [
        // 非公開の関数を指す公開の関数ポインタを、同じ名前で再公開する。
        (
            "",
            "mod child;\nfn translate(_: u8) {}\npub mod api {\n    pub const CALL: fn(u8) = super::translate;\n    pub use CALL as translate;\n}\n",
            "mod child;\nfn translate(_: u8, _: u8) {}\npub mod api {\n    pub const CALL: fn(u8, u8) = super::translate;\n    pub use CALL as translate;\n}\n",
            "use a::api::*;\npub fn run() {\n    translate(1);\n}\n",
            "→ b/src/lib.rs:3 [translate]",
        ),
        // 関数の本体の中で呼んだマクロが、`b` で展開する `#[macro_export]` のマクロを定義する。
        (
            "macro_rules! install {\n    ($($p:tt)*) => {\n        #[macro_export]\n        macro_rules! make_api {\n            () => { pub fn translate($($p)*) {} };\n        }\n    };\n}\n",
            "#[macro_use]\nmod macros;\nmod child;\nfn translate(_: u8) {\n    install!(_: u8);\n}\n",
            "#[macro_use]\nmod macros;\nmod child;\nfn translate(_: u8, _: u8) {\n    install!(_: u8, _: u8);\n}\n",
            "pub mod generated {\n    a::make_api!();\n}\nuse generated::*;\npub fn run() {\n    translate(1);\n}\n",
            "→ b/src/lib.rs:6 [translate]",
        ),
    ] {
        let mut case = Case::new(A_OLD)
            .depends("b", "a = { path = \"../a\" }\n")
            .file("a/src/macros.rs", macros);
        case.a_old = a_old.to_owned();
        case.a_new = a_new.to_owned();
        case.b_lib = b_lib;
        case.files[0].1 = String::new();
        let (_, lines) = case.run();
        assert!(lines.contains(&line.to_owned()), "{a_old}\n{lines:?}");
    }
}

/// `macro_rules!` の本体の名前は展開した場所で解決する。`a` と `b` の両方に依存する第三のクレートが、
/// `a` の公開の別名の見える位置で `b` のマクロを展開すると、本体の `translate(..)` は `a` の関数に着く。
/// 展開先 (木の外のクレート、git の写しなど) は数え切れないので、本体の参照は除かない。proc-macro の
/// クレートのコードも使う側で展開される。
#[test]
fn impact_rust_private_function_keeps_calls_expanded_elsewhere() {
    // `b` は glob の import を持つ (ファイルの位置だけでは `translate` の行き先を決めない)。
    const B_EXPANDS: &str = "use std::collections::*;\n\n#[macro_export]\nmacro_rules! call_translate {\n    () => { translate(1) };\n}\n";
    const EXPANDED_LINE: &str = "→ b/src/lib.rs:5 [translate]";
    let case = |client: &'static str| {
        let mut case = Case::new(A_OLD).depends("client", client).file(
            "client/src/lib.rs",
            "use a::api::*;\npub fn run() {\n    b::call_translate!();\n}\n",
        );
        case.a_old = "mod child;\nfn translate(_: u8) {}\npub mod api {\n    pub const CALL: fn(u8) = super::translate;\n    pub use CALL as translate;\n}\n".to_owned();
        case.a_new = "mod child;\nfn translate(_: u8, _: u8) {}\npub mod api {\n    pub const CALL: fn(u8, u8) = super::translate;\n    pub use CALL as translate;\n}\n".to_owned();
        case.b_lib = B_EXPANDS;
        case.files[0].1 = String::new();
        case.members.push("client");
        case
    };
    for client in [
        "a = { path = \"../a\" }\nb = { path = \"../b\" }\n",
        // 木の中の package が `a` に届かなくても、本体は残す。
        "b = { path = \"../b\" }\n",
    ] {
        let (_, lines) = case(client).run();
        assert!(lines.contains(&EXPANDED_LINE.to_owned()), "{lines:?}");
    }
    // proc-macro のクレート (`proc-macro = true` と `crate-type = ["proc-macro"]`)。
    for lib in [
        "\n[lib]\nproc-macro = true\n",
        "\n[lib]\ncrate-type = [\"proc-macro\"]\n",
    ] {
        let (_, lines) = Case::new(A_OLD).depends("b", lib).run();
        assert!(lines.contains(&B_LINE.to_owned()), "{lib}\n{lines:?}");
    }
}

/// `b` が registry の `c` に依存し、`c` の取得元を差し替えて、`a` に依存するローカルの `c`
/// (`pub use a::api::*`) にすると、`b` の `translate(..)` は `a` の公開の別名に着く。差し替えが無ければ
/// registry の `c` は作業ツリーの `a` をコンパイルしないので除く。
#[test]
fn impact_rust_private_function_keeps_calls_when_cargo_may_redirect_dependencies() {
    const B_GLOB: &str = "use c::*;\npub fn run() {\n    translate(1);\n}\n";
    const GLOB_LINE: &str = "→ b/src/lib.rs:3 [translate]";
    let redirected = |b_dependency: &'static str| {
        let mut case = Case::new(A_OLD)
            .depends("b", b_dependency)
            .depends("c", "a = { path = \"../a\" }\n")
            .file("c/src/lib.rs", "pub use a::api::*;\n");
        case.a_old = "mod child;\nfn translate(_: u8) {}\npub mod api {\n    pub const CALL: fn(u8) = super::translate;\n    pub use CALL as translate;\n}\n".to_owned();
        case.a_new = "mod child;\nfn translate(_: u8, _: u8) {}\npub mod api {\n    pub const CALL: fn(u8, u8) = super::translate;\n    pub use CALL as translate;\n}\n".to_owned();
        case.files[0].1 = String::new();
        case.b_lib = B_GLOB;
        case.members.push("c");
        case
    };
    let registry = || redirected("c = \"0.1\"\n");
    // 対照: 差し替えが無ければ、registry の `c` は作業ツリーの `a` を含まない (除く)。
    let (_, lines) = registry().run();
    assert!(!lines.contains(&GLOB_LINE.to_owned()), "{lines:?}");
    // 対照: ローカルの `c` を path で使えば届く。
    let (_, lines) = redirected("c = { path = \"../c\" }\n").run();
    assert!(lines.contains(&GLOB_LINE.to_owned()), "{lines:?}");
    let mut manifest = registry();
    manifest.root_extra = "\n[patch.crates-io]\nc = { path = \"c\" }\n";
    let configs = [
        ("tools/.cargo/config.toml", "../c"),
        ("target/.cargo/config.toml", "../c"),
        ("target/probe/.cargo/config.toml", "../../c"),
        // Cargo の作るディレクトリと同じ名前 (`build`) の、`target` の下の普通のディレクトリ。
        ("target/probe/build/.cargo/config.toml", "../../../c"),
        (
            "target/probe/.cargo/tool/.cargo/config.toml",
            "../../../../c",
        ),
        // 設定の相対パスは `.cargo` を持つディレクトリ (`.git/hooks`) から読む。
        (".git/hooks/.cargo/config.toml", "../../c"),
        (".git/hooks/objects/.cargo/config.toml", "../../../c"),
    ]
    .map(|(path, target)| {
        registry().file(
            path,
            format!("[patch.crates-io]\nc = {{ path = \"{target}\" }}\n"),
        )
    });
    // profile のディレクトリ (`.cargo-lock`) の直下と、その下の深い設定。
    let profile = registry().file("target/debug/.cargo-lock", "").file(
        "target/debug/.cargo/config.toml",
        "[patch.crates-io]\nc = { path = \"../../c\" }\n",
    );
    let profile_child = registry().file("target/debug/.cargo-lock", "").file(
        "target/debug/probe/.cargo/config.toml",
        "[patch.crates-io]\nc = { path = \"../../../c\" }\n",
    );
    // profile の形 (`.cargo-lock` のファイル) のディレクトリの直下の manifest も、走査が読まない package に
    // なり得る。
    let profile_manifest = registry().file("target/tools/probe/.cargo-lock", "").file(
        "target/tools/probe/Cargo.toml",
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    );
    // git の依存は、その先の manifest の `path` が作業ツリーへ戻り得る。
    let git = redirected("c = { git = \"https://example.test/c.git\" }\n");
    // `base` の付いた path は、root からの相対で Cargo が解決する。
    let base = redirected("c = { base = \"workspace\", path = \"c\" }\n");
    for case in configs.into_iter().chain([
        manifest,
        profile,
        profile_child,
        profile_manifest,
        git,
        base,
    ]) {
        let (_, lines) = case.run();
        assert!(lines.contains(&GLOB_LINE.to_owned()), "{lines:?}");
    }
    // `$CARGO_HOME` が木の中にあると、git・registry の依存の checkout が木のファイルになる。
    let mut home = registry();
    home.cargo_home = Some(".cargo-home");
    let (_, lines) = home.run();
    assert!(lines.contains(&GLOB_LINE.to_owned()), "{lines:?}");
    // 対照: 参照元が依存しない package の差し替えや別の workspace は関係しない (`b` の普通の関数の
    // 中の自分の `translate`)。
    for case in [
        Case::new(A_OLD).depends("a", "mac = { version = \"0.1\", package = \"b\" }\n"),
        Case::new(A_OLD)
            .file(
                "tools/probe/Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\na = { path = \"../../a\" }\n",
            )
            .file("tools/probe/src/lib.rs", ""),
        Case::new(A_OLD)
            .file(
                "tools/client/Cargo.toml",
                "[package]\nname = \"client\"\nversion = \"0.1.0\"\nedition = \"2024\"\nworkspace = \"../ws\"\n\n[dependencies]\na = { path = \"../../a\" }\n",
            )
            .file("tools/ws/Cargo.toml", "[workspace]\nmembers = [\"../client\"]\nresolver = \"3\"\n")
            .file("tools/client/src/lib.rs", ""),
        Case::new(A_OLD)
            .file(
                "tools/formatter/Cargo.toml",
                "[package]\nname = \"formatter\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\nmembers = [\".\", \"./plugins/../core\"]\n",
            )
            .file("tools/formatter/src/main.rs", "fn main() {}\n")
            .file(
                "tools/formatter/core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .file("tools/formatter/core/src/lib.rs", ""),
    ] {
        assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    }
    // `package.workspace` で root を明示した member も、root の workspace として依存をたどる。
    let explicit = Case::new(A_OLD).file(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\nworkspace = \"..\"\n",
    );
    assert_eq!(explicit.run(), (1, vec![CHILD_LINE.to_owned()]));
}

#[test]
fn impact_rust_private_function_keeps_calls_in_nested_workspaces() {
    // 外側の workspace から `inner` を外し、`inner` を `a`・`b` の workspace にする。Cargo は内側の
    // `[workspace.dependencies]` (`mac` は `b`) を使う。
    let repo = TestRepo::new();
    repo.create_dir_all("inner");
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = []\nexclude = [\"inner\"]\nresolver = \"3\"\n\n[workspace.dependencies]\nmac = \"1\"\n",
    );
    repo.write(
        "inner/Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"3\"\n\n[workspace.dependencies]\nmac = { package = \"b\", path = \"b\" }\n",
    );
    for (name, dependencies) in [("a", "mac.workspace = true\n"), ("b", "")] {
        repo.create_dir_all(format!("inner/{name}/src"));
        repo.write(
            format!("inner/{name}/Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}"),
        );
    }
    let a_old =
        format!("{A_OLD}\npub fn via_macro() -> Option<u32> {{\n    mac::call_translate!()\n}}\n");
    repo.write("inner/a/src/lib.rs", &a_old);
    repo.write("inner/a/src/child.rs", A_CHILD);
    repo.write("inner/b/src/lib.rs", B_MACRO);
    repo.init_git();
    repo.commit_all("initial");
    repo.write("inner/a/src/lib.rs", shrink(&a_old));
    let output = cargo_bin()
        .current_dir(repo.root())
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("→ inner/b/src/lib.rs:13 [translate]"),
        "{stderr}"
    );
}

#[test]
fn impact_rust_private_function_keeps_sources_compiled_into_the_owner() {
    // `b/src/calls.rs` は `b` のモジュールで、`a` の取り込みでも `a` の中でコンパイルされる (旧い
    // `a` の `translate(code, test)` で通り、新しい版で壊れる呼び出し)。
    let b_lib = "mod calls;\n\nfn translate(_: u32, _: bool) -> Option<u32> {\n    None\n}\n";
    let calls = "pub fn run() {\n    crate::translate(1, false);\n}\n";
    const CALLS_LINE: &str = "→ b/src/calls.rs:2 [translate]";
    let attach = |rule: &str| {
        format!(
            "macro_rules! attach {{\n    {rule}\n}}\nattach!(path = \"../../b/src/calls.rs\");\n"
        )
    };
    /// (`a/src/lib.rs` の先頭に足す行, ほかのファイル, `a/Cargo.toml` に足す表)。
    type Placement = (String, Vec<(&'static str, String)>, &'static str);
    let cases: Vec<Placement> = vec![
        // `.rs` でないファイルを `#[path]` で読み、その中で `include!` する。
        (
            "#[path = \"../../shared/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![(
                "shared/bridge.inc",
                "include!(\"../b/src/calls.rs\");\n".to_owned(),
            )],
            "",
        ),
        // `.rs` でないファイルの中のマクロの呼び出しが、隣の `calls.rs` を子の `mod` にする。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "children!();\n".to_owned())],
            "",
        ),
        // `#` と属性の名前をメタ変数で渡す (`mount!(#, path)` は `#[path = "..."] mod copied;`)。
        (
            "macro_rules! mount {\n    ($h:tt, $p:ident) => { $h [$p = \"../../b/src/calls.rs\"] mod copied; };\n}\nmount!(#, path);\n".to_owned(),
            vec![],
            "",
        ),
        // 別名だけを置いたファイル (`aliases.rs`) を経て呼ぶ。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children;\nmod aliases;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                ("a/src/aliases.rs", "pub(crate) use crate::children as mount;\n".to_owned()),
                ("b/src/bridge.inc", "crate::aliases::mount!();\n".to_owned()),
            ],
            "",
        ),
        // 角括弧を別のマクロへ渡してから `mod` を付ける (`emit!` の定義に `#` も角括弧も無い)。
        // 変更元とは別のモジュールのファイルで組み立てる。
        (
            "mod mounts;\n".to_owned(),
            vec![(
                "a/src/mounts.rs",
                "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! mount {\n    ($h:tt, $p:ident) => { emit!($h, [$p = \"../../b/src/calls.rs\"]); };\n}\nmount!(#, path);\n".to_owned(),
            )],
            "",
        ),
        // `#` だけを、`path`・`include`・`macro_rules` の綴りの無いファイルから渡す。
        (
            "#[macro_use]\nmod mounts;\nmod wraps;\n".to_owned(),
            vec![
                (
                    "a/src/mounts.rs",
                    "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! mount {\n    ($h:tt, $p:ident) => { emit!($h, [$p = \"../../b/src/calls.rs\"]); };\n}\nmacro_rules! wrap {\n    ($h:tt) => { mount!($h, path); };\n}\n".to_owned(),
                ),
                ("a/src/wraps.rs", "wrap!(#);\n".to_owned()),
            ],
            "",
        ),
        // 引数の中の `quote!(#)` は展開されず、`#` は外側の `wrap!` が受け取る。
        (
            "mod mounts;\n".to_owned(),
            vec![(
                "a/src/mounts.rs",
                "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! wrap {\n    (quote ! ($h:tt), $p:ident) => { emit!($h, [$p = \"../../b/src/calls.rs\"]); };\n}\nwrap!(quote!(#), path);\n".to_owned(),
            )],
            "",
        ),
        // `[` の続かない `# !` も、`path` の綴りの無いファイルから渡す。
        (
            "#[macro_use]\nmod mounts;\nmod wraps;\n".to_owned(),
            vec![
                (
                    "a/src/mounts.rs",
                    "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! mount {\n    ($h:tt, $p:ident) => { emit!($h, [$p = \"../../b/src/calls.rs\"]); };\n}\nmacro_rules! wrap {\n    ($h:tt !) => { mount!($h, path); };\n}\n".to_owned(),
                ),
                ("a/src/wraps.rs", "wrap!(# !);\n".to_owned()),
            ],
            "",
        ),
        // `.inc` の中で、子の `mod` を作るマクロの呼び出しを別のマクロの引数に包む。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\nmacro_rules! pass {\n    ($($t:tt)*) => { $($t)* };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "pass! { children!(); }\n".to_owned())],
            "",
        ),
        // マクロが定義したマクロ (`install!(mount)` が作る `mount`) で、子の `mod` を作る。作られる
        // 名前は分からないので、どのマクロの呼び出しも子の `mod` を作り得るとする。
        (
            "#[macro_use]\nmod macros;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "a/src/macros.rs",
                    "macro_rules! install {\n    ($name:ident) => {\n        macro_rules! $name {\n            () => { mod calls; };\n        }\n    };\n}\ninstall!(mount);\n".to_owned(),
                ),
                ("b/src/bridge.inc", "mount!();\n".to_owned()),
            ],
            "",
        ),
        // 同じことを繰り返し (`$( .. )*`) の直下の `macro_rules!` でする。
        (
            "#[macro_use]\nmod macros;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "a/src/macros.rs",
                    "macro_rules! install {\n    ($($name:ident),*) => {\n        $(macro_rules! $name { () => { mod calls; }; })*\n    };\n}\ninstall!(mount);\n".to_owned(),
                ),
                ("b/src/bridge.inc", "mount!();\n".to_owned()),
            ],
            "",
        ),
        // 子の `mod` を作るマクロの定義を `.rs` でないファイルに置き (`#[macro_use]` と `#[path]`)、
        // 別の `.inc` から呼ぶ。
        (
            "#[macro_use]\n#[path = \"../../shared/defs.inc\"]\nmod defs;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "shared/defs.inc",
                    "macro_rules! children {\n    () => { mod calls; };\n}\n".to_owned(),
                ),
                ("b/src/bridge.inc", "children!();\n".to_owned()),
            ],
            "",
        ),
        // 別名の元の名前の途中にコメントがある (`crate:: /* 注 */ children`)。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children;\nmod aliases;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "a/src/aliases.rs",
                    "pub(crate) use crate:: /* note */ children as mount;\n".to_owned(),
                ),
                ("b/src/bridge.inc", "crate::aliases::mount!();\n".to_owned()),
            ],
            "",
        ),
        // `.rs` でないファイル (`#[path]` で読む) の中で、子の `mod` を作るマクロに別名を付けて呼ぶ。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![(
                "b/src/bridge.inc",
                "use crate::children as\nmount;\nmount!();\n".to_owned(),
            )],
            "",
        ),
        // `mod` もメタ変数で渡す (`mount!(#, path, mod)`)。
        (
            "macro_rules! mount {\n    ($h:tt, $p:ident, $k:ident) => { $h [$p = \"../../b/src/calls.rs\"] $k copied; };\n}\nmount!(#, path, mod);\n".to_owned(),
            vec![],
            "",
        ),
        // 子の `mod` を作るマクロを `use ... as` の別名で呼ぶ。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children as mount;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "crate::mount!();\n".to_owned())],
            "",
        ),
        // `.inc` の中のマクロが、子の `mod` を作るマクロへ委譲する。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\nmacro_rules! forward {\n    () => { children!(); };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "forward!();\n".to_owned())],
            "",
        ),
        // 繰り返しの境界をまたいで組み立てた名前の呼び出し (`$($p::children)? !()`)。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children;\nmacro_rules! forward {\n    ($($p:ident)?) => { $($p::children)? !(); };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "forward!(crate);\n".to_owned())],
            "",
        ),
        // マクロが作る `use .. as` の別名 (`aliases.rs` の `alias!()` が `use crate::children as mount;`)。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children;\nmod aliases;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "a/src/aliases.rs",
                    "macro_rules! alias {\n    () => { pub(crate) use crate::children as mount; };\n}\nalias!();\n".to_owned(),
                ),
                ("b/src/bridge.inc", "crate::aliases::mount!();\n".to_owned()),
            ],
            "",
        ),
        // 同じ workspace の proc-macro のクレートの呼び出し (`mod calls;` を返し得る)。
        (
            "#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "helpers/Cargo.toml",
                    "[package]\nname = \"helpers\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n".to_owned(),
                ),
                (
                    "helpers/src/lib.rs",
                    "use proc_macro::TokenStream;\n#[proc_macro]\npub fn children(_: TokenStream) -> TokenStream {\n    \"mod calls;\".parse().unwrap()\n}\n".to_owned(),
                ),
                ("b/src/bridge.inc", "helpers::children!();\n".to_owned()),
            ],
            "helpers = { path = \"../helpers\" }\n",
        ),
        // 同じ workspace の proc-macro の derive (`mod calls;` を返し得る)。
        (
            "#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "helpers/Cargo.toml",
                    "[package]\nname = \"helpers\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n".to_owned(),
                ),
                (
                    "helpers/src/lib.rs",
                    "use proc_macro::TokenStream;\n#[proc_macro_derive(Children)]\npub fn children(_: TokenStream) -> TokenStream {\n    \"mod calls;\".parse().unwrap()\n}\n".to_owned(),
                ),
                (
                    "b/src/bridge.inc",
                    "#[derive(helpers::Children)]\nstruct Marker;\n".to_owned(),
                ),
            ],
            "helpers = { path = \"../helpers\" }\n",
        ),
        // マクロが作る `use` の `as` が内側の波括弧にある (`use crate::{children as mount};`)。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children;\nmod aliases;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "a/src/aliases.rs",
                    "macro_rules! alias {\n    () => { pub(crate) use crate::{children as mount}; };\n}\nalias!();\n".to_owned(),
                ),
                ("b/src/bridge.inc", "crate::aliases::mount!();\n".to_owned()),
            ],
            "",
        ),
        // マクロの引数の中の `use` の別名 (`aliases.rs` は前置きの絞り込みの綴りを持たない)。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\npub(crate) use children;\nmacro_rules! pass {\n    ($i:item) => { $i };\n}\nmod aliases;\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![
                (
                    "a/src/aliases.rs",
                    "pass! { pub(crate) use crate::children as mount; }\n".to_owned(),
                ),
                ("b/src/bridge.inc", "crate::aliases::mount!();\n".to_owned()),
            ],
            "",
        ),
        // Unicode の識別子の名前のマクロ (`a·b!`) への委譲。
        (
            "macro_rules! a·b {\n    () => { mod calls; };\n}\nmacro_rules! forward {\n    () => { a·b!(); };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "forward!();\n".to_owned())],
            "",
        ),
        // マクロが作る `use` で、自分のマクロに `quote` という名前を付ける (`quote!(#, path)` の `#` は
        // `emit!` で `#[path]` になる)。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! body {\n    ($h:tt, $p:ident) => { emit!($h, [$p = \"../../b/src/calls.rs\"]); };\n}\nmacro_rules! pass {\n    ($i:item) => { $i };\n}\npass! { use body as quote; }\nquote!(#, path);\n".to_owned(),
            )],
            "",
        ),
        // フィールドに見える `loader.include` を、名前を引数から受けて呼ぶマクロ (`$op!($p)`) に渡す。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! load {\n    ($tag:ident . $op:ident, $p:literal) => { $op!($p); };\n}\nload!(loader.include, \"../../b/src/calls.rs\");\n".to_owned(),
            )],
            "",
        ),
        // `!` を別の引数で渡す (`$op $bang $args` は `include!(..)`)。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! load {\n    ($t:ident . $op:ident, $bang:tt, $args:tt) => { $op $bang $args; };\n}\nload!(loader.include, !, (\"../../b/src/calls.rs\"));\n".to_owned(),
            )],
            "",
        ),
        // `#[path]` で読む `.rs` でないファイルの中で、フィールドに見える `include` を `$op!($p)` に
        // 渡す。
        (
            "#[path = \"../../shared/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![(
                "shared/bridge.inc",
                "macro_rules! load {\n    ($tag:ident . $op:ident, $p:literal) => { $op!($p); };\n}\nload!(loader.include, \"../b/src/calls.rs\");\n".to_owned(),
            )],
            "",
        ),
        // マクロが `quote` という名前のマクロを定義する (`quote!(#, path)` の `#` は `emit!` で
        // `#[path]` になる)。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! body {\n    ($h:tt, $p:ident) => { emit!($h, [$p = \"../../b/src/calls.rs\"]); };\n}\nmacro_rules! pass {\n    ($i:item) => { $i };\n}\npass! {\n    macro_rules! quote {\n        ($h:tt, $p:ident) => { body!($h, $p); };\n    }\n}\nquote!(#, path);\n".to_owned(),
            )],
            "",
        ),
        // NFC で同じになる名前 (`caf\u{e9}` と `cafe\u{301}`) のマクロへの委譲。
        (
            "macro_rules! caf\u{e9} {\n    () => { mod calls; };\n}\nmacro_rules! forward {\n    () => { cafe\u{301}!(); };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "forward!();\n".to_owned())],
            "",
        ),
        // 標準の書式のマクロの引数の中の入れ子のマクロ (`value!`) が、フィールドに見える
        // `loader.include` を受け取って展開する。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! value {\n    ($t:ident . $op:ident, $p:literal) => {{\n        mod copied { $op!($p); }\n        0\n    }};\n}\npub fn f() {\n    println!(\"{}\", value!(loader.include, \"../../b/src/calls.rs\"));\n}\n".to_owned(),
            )],
            "",
        ),
        // `quote_spanned!` の先頭の span は展開する式で、中のマクロ (`span!`) が裸の `#` を受け取る。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! span {\n    ($h:tt, $p:ident) => {{\n        emit!($h, [$p = \"../../b/src/calls.rs\"]);\n        ::proc_macro2::Span::call_site()\n    }};\n}\npub fn f() {\n    let _ = quote::quote_spanned!(span!(#, path) => x);\n}\n".to_owned(),
            )],
            "quote = \"1\"\nproc-macro2 = \"1\"\n",
        ),
        // 束縛の無いメタ変数 `$include` は、`$` と `include` に分かれて次のマクロへ渡る。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! take {\n    ($d:tt $op:ident) => { $op!(\"../../b/src/calls.rs\"); };\n}\nmacro_rules! give {\n    () => { take!($include); };\n}\ngive!();\n".to_owned(),
            )],
            "",
        ),
        // 繰り返しの区切りの `include` (`$($x)include*` に 2 つ渡すと間に出力される)。
        (
            "mod child2;\n".to_owned(),
            vec![(
                "a/src/child2.rs",
                "macro_rules! take {\n    ($a:ident $op:ident $b:ident) => { $op!(\"../../b/src/calls.rs\"); };\n}\nmacro_rules! give {\n    ($($x:ident)*) => { take!($($x)include*); };\n}\ngive!(a b);\n".to_owned(),
            )],
            "",
        ),
        // 文法がキーワードとして返す名前のマクロ (`default!`) への委譲。
        (
            "macro_rules! default {\n    () => { mod calls; };\n}\nmacro_rules! forward {\n    () => { default!(); };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "forward!();\n".to_owned())],
            "",
        ),
        // 木の中の proc-macro のクレートは、`#[path]` の無い普通のモジュールの呼び出しからも
        // `include!` を返し得る。
        (
            String::new(),
            vec![
                (
                    "helpers/Cargo.toml",
                    "[package]\nname = \"helpers\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n".to_owned(),
                ),
                (
                    "helpers/src/lib.rs",
                    "use proc_macro::TokenStream;\n#[proc_macro]\npub fn load_calls(_: TokenStream) -> TokenStream {\n    \"include!(concat!(env!(\\\"CARGO_MANIFEST_DIR\\\"), \\\"/../b/src/calls.rs\\\"));\".parse().unwrap()\n}\n".to_owned(),
                ),
                ("a/src/child.rs", format!("{A_CHILD}helpers::load_calls!();\n")),
            ],
            "helpers = { path = \"../helpers\" }\n",
        ),
        // `!` を繰り返しの先頭に置いて委譲する (`forward!(())` は `children!()`)。
        (
            "macro_rules! children {\n    () => { mod calls; };\n}\nmacro_rules! forward {\n    ($($args:tt)?) => { children $(! $args)?; };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "forward!(());\n".to_owned())],
            "",
        ),
        // 繰り返し (`$( .. )*`) の中で委譲する。
        (
            "macro_rules! children {\n    ($n:ident) => { mod $n; };\n}\nmacro_rules! forward {\n    ($($n:ident),*) => { $(children!($n);)* };\n}\n#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n".to_owned(),
            vec![("b/src/bridge.inc", "forward!(calls);\n".to_owned())],
            "",
        ),
        // マクロの展開で `#[path]` を作る (`#[$attr]`・`#[$($t)+]` に `path = ...` を渡す)。
        (
            attach("($attr:meta) => { #[$attr] mod copied; };"),
            Vec::new(),
            "",
        ),
        (
            attach("($($t:tt)+) => { #[$($t)+] mod copied; };"),
            Vec::new(),
            "",
        ),
        // マクロの定義が別のファイルにある (定義には `path` の綴りが無い)。
        (
            "#[macro_use]\nmod macros;\nattach!(path = \"../../b/src/calls.rs\");\n".to_owned(),
            vec![(
                "a/src/macros.rs",
                "macro_rules! attach {\n    ($attr:meta) => { #[$attr] mod copied; };\n}\n".to_owned(),
            )],
            "",
        ),
        // `#` と角括弧を別々に渡す (`# $attr mod copied;` と `[path = ...]`)。
        (
            "macro_rules! mounted {\n    ($attr:tt) => { # $attr mod copied; };\n}\nmounted!([path = \"../../b/src/calls.rs\"]);\n".to_owned(),
            Vec::new(),
            "",
        ),
        // 同じことを繰り返しの中でする (`$( # $br mod copied; )*`)。
        (
            "macro_rules! mounted {\n    ($($br:tt),*) => { $( # $br mod copied; )* };\n}\nmounted!([path = \"../../b/src/calls.rs\"]);\n".to_owned(),
            Vec::new(),
            "",
        ),
        // root の直下の `target` の下の、Cargo の明示の target (走査は中身を読まない)。
        (
            String::new(),
            vec![(
                "target/probe.rs",
                "include!(\"../a/src/lib.rs\");\n#[path = \"../b/src/calls.rs\"]\nmod calls;\nfn main() {}\n".to_owned(),
            )],
            "\n[[bin]]\nname = \"probe\"\npath = \"../target/probe.rs\"\n",
        ),
    ];
    for (prefix, extra, a_manifest) in cases {
        let mut case = Case::new(&format!("{prefix}{A_OLD}")).file("b/src/calls.rs", calls);
        case.b_lib = b_lib;
        if !a_manifest.is_empty() {
            case = case.depends("a", a_manifest);
        }
        for (path, source) in extra {
            case = case.file(path, source);
        }
        let (_, lines) = case.run();
        assert!(
            lines.contains(&CALLS_LINE.to_owned()),
            "{prefix}\n{lines:?}"
        );
    }
    // root の直下の `target` の下の workspace の member (走査は中身を読まない)。
    let mut case = Case::new(A_OLD)
        .file("b/src/calls.rs", calls)
        .file(
            "target/probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file(
            "target/probe/src/lib.rs",
            "include!(\"../../../a/src/lib.rs\");\n#[path = \"../../../b/src/calls.rs\"]\nmod calls;\n",
        );
    case.b_lib = b_lib;
    case.members.push("target/probe");
    let (_, lines) = case.run();
    assert!(lines.contains(&CALLS_LINE.to_owned()), "{lines:?}");
    // 同じ package を、`members` に書かず path の依存で持つ (Cargo は root の下の path の依存を
    // workspace の member にする)。
    let mut case = Case::new(A_OLD)
        .depends("a", "probe = { path = \"../target/probe\" }\n")
        .file("b/src/calls.rs", calls)
        .file(
            "target/probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file(
            "target/probe/src/lib.rs",
            "include!(\"../../../a/src/lib.rs\");\n#[path = \"../../../b/src/calls.rs\"]\nmod calls;\n",
        );
    case.b_lib = b_lib;
    let (_, lines) = case.run();
    assert!(lines.contains(&CALLS_LINE.to_owned()), "{lines:?}");
    // 変更元でない member (`b`) の path の依存も、その package を member にする。
    let mut case = Case::new(A_OLD)
        .depends("b", "probe = { path = \"../target/probe\" }\n")
        .file("b/src/calls.rs", calls)
        .file(
            "target/probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file(
            "target/probe/src/lib.rs",
            "include!(\"../../../a/src/lib.rs\");\n#[path = \"../../../b/src/calls.rs\"]\nmod calls;\n",
        );
    case.b_lib = b_lib;
    let (_, lines) = case.run();
    assert!(lines.contains(&CALLS_LINE.to_owned()), "{lines:?}");
    // 絶対パスの path の依存でも同じ。
    let mut case = Case::new(A_OLD)
        .depends("b", "probe = { path = \"{root}/target/probe\" }\n")
        .file("b/src/calls.rs", calls)
        .file(
            "target/probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file(
            "target/probe/src/lib.rs",
            "include!(\"../../../a/src/lib.rs\");\n#[path = \"../../../b/src/calls.rs\"]\nmod calls;\n",
        );
    case.b_lib = b_lib;
    let (_, lines) = case.run();
    assert!(lines.contains(&CALLS_LINE.to_owned()), "{lines:?}");
    // 先頭の要素の glob が `target` に合い、深い package を選ぶ (`targe?/level/*`)。
    let mut case = Case::new(A_OLD)
        .file("b/src/calls.rs", calls)
        .file(
            "target/level/probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file(
            "target/level/probe/src/lib.rs",
            "include!(\"../../../../a/src/lib.rs\");\n#[path = \"../../../../b/src/calls.rs\"]\nmod calls;\n",
        );
    case.b_lib = b_lib;
    case.members.push("targe?/level/*");
    let (_, lines) = case.run();
    assert!(lines.contains(&CALLS_LINE.to_owned()), "{lines:?}");
    // 同じ glob を `./` から書く。
    let mut case = Case::new(A_OLD)
        .file("b/src/calls.rs", calls)
        .file(
            "target/level/probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file(
            "target/level/probe/src/lib.rs",
            "include!(\"../../../../a/src/lib.rs\");\n#[path = \"../../../../b/src/calls.rs\"]\nmod calls;\n",
        );
    case.b_lib = b_lib;
    case.members.push("./targe?/level/*");
    let (_, lines) = case.run();
    assert!(lines.contains(&CALLS_LINE.to_owned()), "{lines:?}");
    // root の外を回って `target` に戻る path の依存 (`../../<root>/target/probe`)。
    let mut case = Case::new(A_OLD)
        .depends("b", "probe = { path = \"../../{root_name}/target/probe\" }\n")
        .file("b/src/calls.rs", calls)
        .file(
            "target/probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .file(
            "target/probe/src/lib.rs",
            "include!(\"../../../a/src/lib.rs\");\n#[path = \"../../../b/src/calls.rs\"]\nmod calls;\n",
        );
    case.b_lib = b_lib;
    let (_, lines) = case.run();
    assert!(lines.contains(&CALLS_LINE.to_owned()), "{lines:?}");
}

/// `target` の下の package を path の依存で member にすると、非公開の型の配置 (#470 と共通) も
/// 決めない (`probe` は `a/src/lib.rs` と `b/src/calls.rs` を同じクレートに取り込む)。
#[test]
fn impact_rust_private_type_keeps_files_compiled_by_unscanned_members() {
    // `target` の下の `probe` (`target/probe` か `target/level/probe`) は、`a/src/lib.rs` と
    // `b/src/calls.rs` を取り込む。関数の試験と違い、`target` の下の `Cargo.toml` を見つける別の
    // 判定が無いので、path の依存と member の表記の読み方をそれぞれ確かめられる。
    let probe = |level: bool| -> Case<'static> {
        let (manifest, lib, source) = if level {
            (
                "target/level/probe/Cargo.toml",
                "target/level/probe/src/lib.rs",
                "include!(\"../../../../a/src/lib.rs\");\n#[path = \"../../../../b/src/calls.rs\"]\nmod calls;\n",
            )
        } else {
            (
                "target/probe/Cargo.toml",
                "target/probe/src/lib.rs",
                "include!(\"../../../a/src/lib.rs\");\n#[path = \"../../../b/src/calls.rs\"]\nmod calls;\n",
            )
        };
        let mut case = Case::new(A_OLD)
            .file(
                "b/src/calls.rs",
                "use super::*;\nfn take(_: &Settings<u8>) {}\n",
            )
            .file(
                manifest,
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .file(lib, source);
        case.a_old = "mod child;\nstruct Settings<T>(T);\n".to_owned();
        case.a_new = "mod child;\nstruct Settings<T, U>(T, U);\n".to_owned();
        case.b_lib = "mod calls;\nstruct Settings<T>(T);\n";
        case.files[0].1 = String::new();
        case
    };
    let relative = probe(false).depends("b", "probe = { path = \"../target/probe\" }\n");
    // `[patch]` で `target` の下の package を選ぶ (`b` は registry の `probe` に依存する)。
    let mut patch = probe(false).depends("b", "probe = \"=0.1.0\"\n");
    patch.root_extra = "\n[patch.crates-io]\nprobe = { path = \"target/probe\" }\n";
    // 通常の絶対パス (Windows の `canonicalize` した root には `\\?\` が付く)。
    let absolute = probe(false).depends("b", "probe = { path = \"{root}/target/probe\" }\n");
    // root の外を回って戻るパス。
    let around = probe(false).depends(
        "b",
        "probe = { path = \"../../{root_name}/target/probe\" }\n",
    );
    // `..` を含む member と、先頭の要素が glob の member (`./` から書いたものも)。
    let mut dotted = probe(false);
    dotted.members.push("a/../target/probe");
    let mut glob = probe(true);
    glob.members.push("targe?/level/*");
    let mut dot_glob = probe(true);
    dot_glob.members.push("./targe?/level/*");
    // 末尾の区切りはディレクトリの指定 (`targe?/level/*/`)。
    let mut slash_glob = probe(true);
    slash_glob.members.push("targe?/level/*/");
    // 途中の `./` は無視する (`targe?/./level/*`)。
    let mut mid_dot_glob = probe(true);
    mid_dot_glob.members.push("targe?/./level/*");
    for (label, case) in [
        ("relative", relative),
        ("patch", patch),
        ("absolute", absolute),
        ("around", around),
        ("dotted member", dotted),
        ("glob member", glob),
        ("dot glob member", dot_glob),
        ("slash glob member", slash_glob),
        ("mid dot glob member", mid_dot_glob),
    ] {
        let (_, lines) = case.run();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("→ b/src/calls.rs:2 [")),
            "{label}: {lines:?}"
        );
    }
}

/// 変更元とは別のモジュールのファイルで、マクロを経て組み立てる `#[path]` (`mount!(#, path)` が
/// `emit!($h, [$p = "..."])` を経て `$h $br mod copied;`) は、非公開の型の配置 (#470 と共通) も決めない。
#[test]
fn impact_rust_private_type_keeps_files_mounted_by_assembled_attributes() {
    let mut case = Case::new(A_OLD)
        .file("b/src/calls.rs", "fn take(_: &crate::Settings<u8>) {}\n")
        .file(
            "a/src/mounts.rs",
            "macro_rules! emit {\n    ($h:tt, $br:tt) => { $h $br mod copied; };\n}\nmacro_rules! mount {\n    ($h:tt, $p:ident) => { emit!($h, [$p = \"../../b/src/calls.rs\"]); };\n}\nmount!(#, path);\n",
        );
    case.a_old = "mod mounts;\nstruct Settings<T>(T);\n".to_owned();
    case.a_new = "mod mounts;\nstruct Settings<T, U>(T, U);\n".to_owned();
    case.b_lib = "mod calls;\nstruct Settings<T>(T);\n";
    case.files[0].1 = String::new();
    let (_, lines) = case.run();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("→ b/src/calls.rs:1 [")),
        "{lines:?}"
    );
}

/// 普通のマクロ (`println!`) と組み込みの derive だけの `#[path]` のモジュールは、別のクレートの
/// 除外を止めない。木の中に `macro_rules!` があれば (配列の式を返すだけのマクロでも、マクロを
/// 定義するマクロでも) 除かない。
#[test]
fn impact_rust_private_function_keeps_excluding_with_plain_path_modules() {
    let case = Case::new(&format!("#[path = \"helper.rs\"]\nmod helper;\n{A_OLD}")).file(
        "a/src/helper.rs",
        "pub fn note() {\n    println!(\"note\");\n}\n",
    );
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    let case = Case::new(&format!("#[path = \"helper.rs\"]\nmod helper;\n{A_OLD}"))
        .file("a/src/helper.rs", "#[derive(Debug)]\npub struct Marker;\n");
    assert_eq!(case.run(), (1, vec![CHILD_LINE.to_owned()]));
    for (path, source) in [
        (
            "b/src/one.rs",
            "macro_rules! one {\n    ($x:expr) => { [$x] };\n}\n",
        ),
        (
            "b/src/install.rs",
            "macro_rules! install {\n    () => { macro_rules! noop { () => {}; } };\n}\n",
        ),
    ] {
        let case = Case::new(&format!("#[path = \"helper.rs\"]\nmod helper;\n{A_OLD}"))
            .file("a/src/helper.rs", "#[derive(Debug)]\npub struct Marker;\n")
            .file(path, source);
        assert_eq!(
            case.run(),
            (
                1,
                vec![
                    CHILD_LINE.to_owned(),
                    "→ b/src/lib.rs:4 [translate]".to_owned()
                ]
            ),
            "{source}"
        );
    }
    // `r#` を付けて宣言しても、参照は `r#` を外した名前で引く (除外が効く)。影響の行には宣言の
    // 綴りの名前が出る。
    let raw = A_OLD.replace("fn translate(code", "fn r#translate(code");
    assert_ne!(raw, A_OLD);
    assert_eq!(
        Case::new(&raw).run(),
        (1, vec![CHILD_LINE.replace("[translate]", "[r#translate]")])
    );
}

/// `#[path]` の先が symlink なら、rustc は書いたパスの親 (`b/src`) から子の `mod` を探す。
#[cfg(unix)]
#[test]
fn impact_rust_private_function_keeps_children_of_symlinked_path_targets() {
    let b_lib = "mod calls;\n\nfn translate(_: u32, _: bool) -> Option<u32> {\n    None\n}\n";
    let calls = "pub fn run() {\n    crate::translate(1, false);\n}\n";
    let repo = TestRepo::new();
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"3\"\n",
    );
    for name in ["a", "b"] {
        repo.create_dir_all(format!("{name}/src"));
        repo.write(
            format!("{name}/Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        );
    }
    let a_old = format!("#[path = \"../../b/src/bridge.inc\"]\nmod bridge;\n{A_OLD}");
    repo.write("a/src/lib.rs", &a_old);
    repo.write("a/src/child.rs", A_CHILD);
    repo.write("b/src/lib.rs", b_lib);
    repo.write("b/src/calls.rs", calls);
    repo.create_dir_all("shared");
    repo.write("shared/body.inc", "mod calls;\n");
    std::os::unix::fs::symlink(
        "../../shared/body.inc",
        repo.root().join("b/src/bridge.inc"),
    )
    .unwrap();
    repo.init_git();
    repo.commit_all("initial");
    repo.write("a/src/lib.rs", shrink(&a_old));
    let output = cargo_bin()
        .current_dir(repo.root())
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("→ b/src/calls.rs:2 [translate]"),
        "{stderr}"
    );
}

/// 大文字小文字を区別するファイルシステムでは、`Bridge` と `bridge` は別の package。探索済みの印を
/// 綴りで畳むと、`bridge` の先の `b` に届くことを見落とす。区別しないファイルシステムでは試さない。
#[test]
fn impact_rust_private_function_follows_case_distinct_packages() {
    let probe = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(probe.path().join("Probe")).unwrap();
    if std::fs::create_dir(probe.path().join("probe")).is_err() {
        return;
    }
    // `a` → `Bridge` (package `bridge_one`) → `bridge` (package `bridge_two`) → `b` と一列に依存し、
    // 中間の 2 つが `b` のマクロを再輸出する。
    let mut case = Case::new(&format!(
        "{A_OLD}\npub fn via_macro() -> Option<u32> {{\n    bridge_one::call_translate!()\n}}\n"
    ))
    .depends("a", "bridge_one = { path = \"../Bridge\" }\n")
    .file(
        "Bridge/Cargo.toml",
        "[package]\nname = \"bridge_one\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nbridge_two = { path = \"../bridge\" }\n",
    )
    .file("Bridge/src/lib.rs", "pub use bridge_two::call_translate;\n")
    .file(
        "bridge/Cargo.toml",
        "[package]\nname = \"bridge_two\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n",
    )
    .file("bridge/src/lib.rs", "pub use b::call_translate;\n");
    case.b_lib = B_MACRO;
    case.members.extend(["Bridge", "bridge"]);
    let (_, lines) = case.run();
    assert!(lines.contains(&B_MACRO_LINE.to_owned()), "{lines:?}");
}

#[test]
fn impact_rust_private_function_keeps_calls_when_cfg_changes() {
    // 無効だった非公開の関数を有効にすると、同じ名前の公開の glob の再輸出を隠し、`a` に依存する
    // `b` の `a::translate(1)` が非公開の関数を指して壊れる。
    let a_old = "pub mod api;\npub use api::*;\n\n#[cfg(any())]\nfn translate(_: u8) {}\n";
    let a_new = "pub mod api;\npub use api::*;\n\n#[cfg(all())]\nfn translate() {}\n";
    let mut case = Case::new(A_OLD)
        .depends("b", "a = { path = \"../a\" }\n")
        .file("a/src/api.rs", "pub fn translate(_: u8) {}\n");
    case.a_old = a_old.to_owned();
    case.a_new = a_new.to_owned();
    case.files.retain(|(path, _)| *path != "a/src/child.rs");
    case.b_lib = "pub fn run() {\n    a::translate(1);\n}\n";
    let (_, lines) = case.run();
    assert!(
        lines.contains(&"→ b/src/lib.rs:2 [translate]".to_owned()),
        "{lines:?}"
    );
    // 属性の文字列の中の空白だけが違っても別の条件 (`cfg` の値は文字列そのもの)。
    let mut case = Case::new(&A_OLD.replace(
        "fn translate(",
        "#[cfg(not(feature = \"a  b\"))]\nfn translate(",
    ));
    case.a_new = shrink(&A_OLD.replace(
        "fn translate(",
        "#[cfg(not(feature = \"a b\"))]\nfn translate(",
    ));
    let (_, lines) = case.run();
    assert!(lines.contains(&B_LINE.to_owned()), "{lines:?}");
}

/// `translate` と `r#translate` は refs で同じ名前の参照になるので、非公開の `translate` は同じ
/// ファイルの公開の `r#translate` と区別できない (公開の側の呼び出しを除かない)。
#[test]
fn impact_rust_private_function_counts_raw_identifiers_as_the_same_name() {
    let a_old = "fn translate(_: u8) {}\n\npub mod api {\n    pub fn r#translate(_: u8) {}\n}\n";
    let a_new = "fn translate() {}\n\npub mod api {\n    pub fn r#translate() {}\n}\n";
    let mut case = Case::new(A_OLD).depends("b", "a = { path = \"../a\" }\n");
    case.a_old = a_old.to_owned();
    case.a_new = a_new.to_owned();
    case.files.retain(|(path, _)| *path != "a/src/child.rs");
    case.b_lib = "pub fn run() {\n    a::api::translate(1);\n}\n";
    let (_, lines) = case.run();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("→ b/src/lib.rs:2 [")),
        "{lines:?}"
    );
    // 本体の無い `extern` の関数の宣言も同じ名前の宣言。子のモジュール
    // (`mod child;`) を残し、ファイルの外の呼び出しを探す形にする。
    let mut case = Case::new(A_OLD).depends("b", "a = { path = \"../a\" }\n");
    case.a_old = "mod child;\nfn translate(_: u8) {}\n\npub mod api {\n    unsafe extern \"C\" {\n        pub fn translate(x: u8);\n    }\n}\n".to_owned();
    case.a_new = "mod child;\nfn translate() {}\n\npub mod api {\n    unsafe extern \"C\" {\n        pub fn translate();\n    }\n}\n".to_owned();
    case.b_lib = "pub fn run() {\n    unsafe { a::api::translate(1); }\n}\n";
    let (_, lines) = case.run();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("→ b/src/lib.rs:2 [")),
        "{lines:?}"
    );
    // 旧い版だけ・新しい版だけに `extern` の宣言があっても、名前は一意でない。
    let with_extern = "mod child;\nfn translate(_: u8) {}\n\npub mod api {\n    unsafe extern \"C\" {\n        pub fn translate(x: u8);\n    }\n}\n";
    let without_extern = "mod child;\nfn translate() {}\n\npub mod api {}\n";
    for (a_old, a_new) in [
        (with_extern.to_owned(), without_extern.to_owned()),
        (
            without_extern.replace("fn translate()", "fn translate(_: u8, _: u8)"),
            with_extern.to_owned(),
        ),
    ] {
        let mut case = Case::new(A_OLD).depends("b", "a = { path = \"../a\" }\n");
        case.a_old = a_old;
        case.a_new = a_new;
        case.b_lib = "pub fn run() {\n    unsafe { a::api::translate(1); }\n}\n";
        let (_, lines) = case.run();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("→ b/src/lib.rs:2 [")),
            "{lines:?}"
        );
    }
}

/// 同じファイルの同じ名前の宣言を、シンボルの抽出が数えなくても数える (公開の enum の variant は
/// `use a::Op::*;` の先で `translate(1)` と呼べる。マクロが `pub fn translate` を作る)。
#[test]
fn impact_rust_private_function_counts_unlisted_declarations() {
    // 別のファイルで定義した、名前を引数で受け取って宣言するマクロ。
    let by_argument =
        "macro_rules! generate {\n    ($n:ident, ($($p:tt)*)) => { pub fn $n($($p)*) {} };\n}\n";
    for (macros, a_old, a_new, b_lib) in [
        (
            "",
            "mod child;\nfn translate(_: u8) {}\n#[allow(non_camel_case_types)]\npub enum Op {\n    translate(u8),\n}\n",
            "mod child;\nfn translate() {}\n#[allow(non_camel_case_types)]\npub enum Op {\n    translate(u8, u8),\n}\n",
            "use a::Op::*;\npub fn run() {\n    let _ = translate(1);\n}\n",
        ),
        (
            "",
            "mod child;\nfn translate(_: u8) {}\nmacro_rules! generate {\n    () => { pub fn translate(_: u8) {} };\n}\npub mod api {\n    generate!();\n}\n",
            "mod child;\nfn translate() {}\nmacro_rules! generate {\n    () => { pub fn translate(_: u8, _: u8) {} };\n}\npub mod api {\n    generate!();\n}\n",
            "use a::api::*;\npub fn run() {\n    let _ = translate(1);\n}\n",
        ),
        (
            "",
            "mod child;\nfn translate(_: u8) {}\nmacro_rules! generate {\n    ($n:ident) => { pub fn $n(_: u8) {} };\n}\npub mod api {\n    generate!(translate);\n}\n",
            "mod child;\nfn translate() {}\nmacro_rules! generate {\n    ($n:ident) => { pub fn $n(_: u8, _: u8) {} };\n}\npub mod api {\n    generate!(translate);\n}\n",
            "use a::api::*;\npub fn run() {\n    let _ = translate(1);\n}\n",
        ),
        (
            by_argument,
            "#[macro_use]\nmod macros;\nmod child;\nfn translate(_: u8) {}\npub mod api {\n    generate!(translate, (_: u8));\n}\n",
            "#[macro_use]\nmod macros;\nmod child;\nfn translate() {}\npub mod api {\n    generate!(translate, (_: u8, _: u8));\n}\n",
            "use a::api::*;\npub fn run() {\n    let _ = translate(1);\n}\n",
        ),
        // 標準のマクロの名前 (`vec`) で再公開したマクロを、パスで呼ぶ。
        (
            "macro_rules! generate {\n    ($n:ident, ($($p:tt)*)) => { pub fn $n($($p)*) {} };\n}\npub(crate) use generate as vec;\n",
            "mod macros;\nmod child;\nfn translate(_: u8) {}\npub mod api {\n    crate::macros::vec!(translate, (_: u8));\n}\n",
            "mod macros;\nmod child;\nfn translate() {}\npub mod api {\n    crate::macros::vec!(translate, (_: u8, _: u8));\n}\n",
            "use a::api::*;\npub fn run() {\n    let _ = translate(1);\n}\n",
        ),
        // 名前を本体に固定したマクロ (呼び出しに `translate` が現れない)。
        (
            "macro_rules! generate {\n    ($($p:tt)*) => { pub fn translate($($p)*) {} };\n}\n",
            "#[macro_use]\nmod macros;\nmod child;\nfn translate(_: u8) {}\npub mod api {\n    generate!(_: u8);\n}\n",
            "#[macro_use]\nmod macros;\nmod child;\nfn translate() {}\npub mod api {\n    generate!(_: u8, _: u8);\n}\n",
            "use a::api::*;\npub fn run() {\n    let _ = translate(1);\n}\n",
        ),
    ] {
        let mut case = Case::new(A_OLD)
            .depends("b", "a = { path = \"../a\" }\n")
            .file("a/src/macros.rs", macros);
        case.a_old = a_old.to_owned();
        case.a_new = a_new.to_owned();
        case.b_lib = b_lib;
        // 子は旧い `translate(_: u8)` と合わない呼び出しを持たない (変更前もコンパイルできる形)。
        case.files[0].1 = String::new();
        let (_, lines) = case.run();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("→ b/src/lib.rs:3 [")),
            "{a_new}\n{lines:?}"
        );
    }
}

/// `r#` を付けた非公開の inherent メソッドも、所有型を引いて子のモジュールの呼び出しを残す
/// (宣言の数え方を `r#` を外した名前にそろえた後も、所有型の数え方が同じ名前で引く)。
#[test]
fn impact_rust_private_raw_method_keeps_child_callers() {
    let mut case = Case::new(A_OLD);
    case.a_old =
        "mod child;\npub struct S;\nimpl S {\n    fn r#translate(_: u8) {}\n}\n".to_owned();
    case.a_new = "mod child;\npub struct S;\nimpl S {\n    fn r#translate() {}\n}\n".to_owned();
    case.files = vec![(
        "a/src/child.rs",
        "pub fn run() {\n    super::S::translate(1);\n}\n".to_owned(),
    )];
    let (_, lines) = case.run();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("→ a/src/child.rs:2 [")),
        "{lines:?}"
    );
}
