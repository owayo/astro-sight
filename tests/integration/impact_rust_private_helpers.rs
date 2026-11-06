//! private helper と別の所有元の同名参照 (Issue #133)。

use super::support::*;

#[test]
fn impact_rust_independent_root_values_ignore_initializer_mentions() {
    for constant in [true, false] {
        let repo = TestRepo::new();
        repo.create_dir_all("src/changed");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"bindings\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write(
            "src/lib.rs",
            "mod changed; mod local; mod real;\nmacro_rules! marker { () => {} }\nmarker!();\n",
        );
        // 定数の側は型を変える。初期値だけの変更は、関数の本体で値を読むだけの参照を情報提供に
        // 回す (Issue #257) ので、本当の呼び出し元が impacted_callers に残る対照にならない。
        let (before, after, local, real) = if constant {
            (
                "mod child;\nconst KNOWN_FLAGS: u8 = 1;\n",
                "mod child;\nconst KNOWN_FLAGS: u16 = 3;\n",
                "const KNOWN_FLAGS: u8 = 1;\npub fn local() -> u8 { let result = KNOWN_FLAGS; if matches!(result, 0) { 0 } else { result } }\n#[cfg(test)] mod tests {}\n",
                "pub fn real() {}\n",
            )
        } else {
            (
                "pub(crate) fn normalize(x: u8) -> u8 { x }\n",
                "pub(crate) fn normalize(x: u16, _: bool) -> u16 { x }\n",
                "fn normalize(x: u8) -> u8 { x }\npub fn local() -> u8 { let result = normalize(1); if matches!(result, 0) { 0 } else { result } }\n#[cfg(test)] mod tests {}\n",
                "pub fn real() -> u8 { crate::changed::normalize(1) }\n",
            )
        };
        repo.write("src/changed.rs", before);
        if constant {
            repo.write(
                "src/changed/child.rs",
                "pub fn real() -> u8 { super::KNOWN_FLAGS }\n",
            );
        }
        repo.write("src/local.rs", local);
        repo.write("src/real.rs", real);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/changed.rs", after);
        let context = repo.run_json("context", &["--git"]);
        let has_caller = |bucket: &str, path: &str| {
            context["changes"].as_array().unwrap().iter().any(|change| {
                change[bucket]
                    .as_array()
                    .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == path))
            })
        };
        assert!(!has_caller("impacted_callers", "src/local.rs"), "{context}");
        let true_caller = if constant {
            "src/changed/child.rs"
        } else {
            "src/real.rs"
        };
        assert!(has_caller("impacted_callers", true_caller), "{context}");
    }
}

#[test]
fn impact_rust_enum_variants_have_their_own_declaration_owner() {
    for case in [
        "plain",
        "alias",
        "tuple",
        "generic",
        "type alias",
        "conditional",
        "path",
        "variant cfg",
        "attribute",
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"variants\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        let module = match case {
            "conditional" => "#[cfg(feature = \"enabled\")] mod kind;",
            "path" => "#[path = \"other.rs\"] mod kind;",
            _ => "mod kind;",
        };
        repo.write("src/lib.rs", format!("mod changed; {module} mod caller; mod real;\nmacro_rules! marker {{ () => {{}} }}\nmarker!();\n"));
        let attribute = if case == "attribute" {
            "#[transform]"
        } else {
            "#[derive(Clone, Debug)]"
        };
        let variant = if case == "tuple" {
            "Reference(u8)"
        } else if case == "variant cfg" {
            "#[cfg(feature = \"enabled\")] Reference"
        } else {
            "Reference"
        };
        repo.write(
            "src/kind.rs",
            format!("{attribute}\npub enum Kind {{ {variant}, Other }}\n"),
        );
        repo.write("src/other.rs", "pub enum Kind { Reference, Other }\n");
        let caller = match case {
            "alias" => "use crate::kind::Kind as K;\npub fn other() -> K { K::Reference }\n",
            "generic" => "pub fn other<Kind>() { let value = Kind::Reference; }\n",
            "type alias" => {
                "type Kind = crate::kind::Kind;\npub fn other() -> Kind { Kind::Reference }\n"
            }
            "tuple" => "use crate::kind::{Kind};\npub fn other() -> Kind { Kind::Reference(1) }\n",
            _ => {
                "use crate::kind::{Kind};\npub fn other() -> Kind { let result = Kind::Reference; result }\n"
            }
        };
        repo.write("src/caller.rs", caller);
        repo.write(
            "src/real.rs",
            "pub fn real() -> crate::changed::Reference { crate::changed::Reference(1) }\n",
        );
        repo.write(
            "src/changed.rs",
            "pub(crate) struct Reference(pub(crate) u8);\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        let refs_before = repo.run_json("refs", &["--name", "Reference"]);
        repo.write(
            "src/changed.rs",
            "pub(crate) struct Reference(pub(crate) u16);\n",
        );
        let context = repo.run_json("context", &["--git"]);
        let has = |bucket: &str, path: &str| {
            context["changes"].as_array().unwrap().iter().any(|change| {
                change[bucket]
                    .as_array()
                    .is_some_and(|callers| callers.iter().any(|caller| caller["path"] == path))
            })
        };
        assert!(has("impacted_callers", "src/real.rs"), "{case}: {context}");
        assert_eq!(
            has("impacted_callers", "src/caller.rs"),
            !matches!(case, "plain" | "alias" | "tuple"),
            "{case}: {context}"
        );
        let refs_after = repo.run_json("refs", &["--name", "Reference"]);
        let uses = |result: &serde_json::Value| {
            result["refs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["kind"] == "ref")
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(uses(&refs_before), uses(&refs_after), "{case}");
    }
}

#[test]
fn impact_rust_format_arguments_require_standard_macro_binding() {
    for case in [
        "plain",
        "root shadow",
        "parent shadow",
        "parent import",
        "parent glob",
        "local shadow",
        "import",
        "block",
        "nested macro",
        "qualified",
        "config",
        "self import",
        "unrelated path",
        "assert variable",
        "assert shadow",
        "include source",
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"format_bindings\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write(
            "src/main.rs",
            "mod changed; mod real;\nfn main() { let _ = real::real(); }\n",
        );
        let macro_definition = "macro_rules! format { ($($args:tt)*) => { String::new() }; }\n";
        repo.write(
            "src/lib.rs",
            format!(
                "{}mod changed; mod local; mod real;\n",
                if case == "parent shadow" {
                    macro_definition
                } else {
                    ""
                }
            ),
        );
        if matches!(
            case,
            "unrelated path" | "assert variable" | "assert shadow" | "include source"
        ) {
            repo.write(
                "src/lib.rs",
                "mod changed; mod local; mod real; mod extra;\n",
            );
            match case {
                "unrelated path" => {
                    repo.write("src/lib.rs", "mod changed; mod local; mod real; #[path = \"item.rs\"] mod extra;\n");
                    repo.create_dir_all("src/relocated");
                    repo.write("src/extra.rs", "#[path = \"item.rs\"] mod item;\n");
                    repo.write("src/item.rs", "pub fn item() {}\n");
                }
                "include source" => repo.write("src/extra.rs", "include!(\"external.rs\");\n"),
                "assert shadow" => repo.write("src/extra.rs", "macro_rules! assert { ($($args:tt)*) => {} }\nfn demo() { let include = |x: bool| x; assert!(include(true)); }\n"),
                _ => repo.write("src/extra.rs", "fn demo() { let include = |x: bool| x; assert!(include(true)); assert!(!include(false)); }\n"),
            }
        }
        if matches!(case, "parent import" | "parent glob") {
            let imported = if case == "parent import" {
                "use crate::macros::format;"
            } else {
                "use crate::macros::*;"
            };
            repo.write(
                "src/lib.rs",
                format!("{imported} mod macros; mod changed; mod local; mod real;\n"),
            );
            repo.write("src/macros.rs", "#[macro_export] macro_rules! replacement_format { ($($args:tt)*) => { String::new() }; }\npub use crate::replacement_format as format;\n");
        }
        let prefix = match case {
            "root shadow" => macro_definition,
            "import" => "use crate::macros::format;\n",
            "self import" => "use std::io::{self, Write};\n",
            _ => "",
        };
        let local = if case == "local shadow" {
            macro_definition
        } else {
            ""
        };
        let argument = match case {
            "block" => "{ let normalize = crate::changed::normalize; normalize(1) }",
            "nested macro" => "custom!(normalize(1))",
            "qualified" => "crate::changed::normalize(1)",
            _ => "normalize(1)",
        };
        let source = format!(
            "{prefix}fn normalize(value: u8) -> u8 {{ value }}\npub fn local() -> Option<String> {{\n{local} Some(format!(\"{{}}{{}} {{}}\", if true {{ \"a\" }} else {{ \"b\" }}, 1, {argument}))\n}}\n#[cfg(test)] mod tests {{}}\n"
        );
        repo.write("src/local.rs", &source);
        repo.write(
            "src/changed.rs",
            "pub(crate) fn normalize(value: u8) -> u8 { value }\n",
        );
        repo.write(
            "src/real.rs",
            "pub fn real() -> u8 { crate::changed::normalize(1) }\n",
        );
        if case == "config" {
            repo.create_dir_all(".cargo");
            repo.write(
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--extern\", \"std=other.rlib\"]\n",
            );
        }
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/changed.rs",
            "pub(crate) fn normalize(value: u16, _: bool) -> u16 { value }\n",
        );
        let context = repo.run_json("context", &["--git"]);
        let line = source
            .lines()
            .position(|line| line.contains("Some(format!"))
            .unwrap();
        let changes = context["changes"].as_array().unwrap();
        let calls = changes[0]["impacted_callers"].as_array().unwrap();
        assert!(
            calls.iter().any(|caller| caller["path"] == "src/real.rs"),
            "{case}: {context}"
        );
        assert_eq!(
            calls
                .iter()
                .any(|caller| caller["path"] == "src/local.rs" && caller["line"] == line),
            !matches!(
                case,
                "plain"
                    | "self import"
                    | "unrelated path"
                    | "assert variable"
                    | "parent import"
                    | "parent glob"
            ),
            "{case}: {context}"
        );
        if case == "plain" {
            repo.write(
                "src/real.rs",
                "pub fn real() -> u8 { crate::changed::normalize(1, true) as u8 }\n",
            );
            for command in ["impact", "review"] {
                let output = cargo_bin()
                    .args([command, "--git", "--hook", "--dir"])
                    .arg(repo.root())
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{command}: {output:?}");
            }
            repo.stage_all();
            let staged = repo.run_json("context", &["--git", "--staged"]);
            assert!(
                staged["changes"].as_array().unwrap().iter().any(|change| {
                    change["impacted_callers"].as_array().is_some_and(|calls| {
                        calls.iter().any(|caller| caller["path"] == "src/local.rs")
                    })
                }),
                "{staged}"
            );
        }
    }
}

/// 文・末尾式の位置の `format!` そのものは束縛を作らない。標準と証明した呼び出しだけを
/// 再束縛の障壁から外し、ほかの文のマクロは障壁に残す (Issue #260)。
#[test]
fn impact_rust_format_arguments_in_statement_positions_use_local_bindings() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"format_statements\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod changed; mod local; mod real;\n");
    repo.write(
        "src/changed.rs",
        "pub(crate) fn normalize(value: u8) -> u8 { value }\n",
    );
    repo.write(
        "src/real.rs",
        "pub fn real() -> u8 { crate::changed::normalize(1) }\n",
    );
    let local = "fn normalize(value: u8) -> u8 {\n    value\n}\n\npub fn tail() -> String {\n    format!(\"{}\", normalize(1))\n}\n\npub fn statement() {\n    format!(\"{}\", normalize(2));\n}\n\npub fn after_macro() -> String {\n    make_binding!();\n    format!(\"{}\", normalize(3))\n}\n";
    repo.write("src/local.rs", local);
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/changed.rs",
        "pub(crate) fn normalize(value: u16, _: bool) -> u16 { value }\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let calls = context["changes"][0]["impacted_callers"]
        .as_array()
        .unwrap();
    let reported = |path: &str, needle: &str| {
        let line = local
            .lines()
            .position(|line| line.contains(needle))
            .unwrap();
        calls
            .iter()
            .any(|caller| caller["path"] == path && caller["line"] == line)
    };
    assert!(
        calls.iter().any(|caller| caller["path"] == "src/real.rs"),
        "{context}"
    );
    assert!(!reported("src/local.rs", "normalize(1)"), "{context}");
    assert!(!reported("src/local.rs", "normalize(2)"), "{context}");
    assert!(reported("src/local.rs", "normalize(3)"), "{context}");
}

fn private_helpers_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src/style");
    repo.create_dir_all("src/config");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "mod config; mod literal; mod style;\nmacro_rules! marker { () => { const MARKER: u8 = 0; } }\nmarker!();\n",
    );
    repo.write(
        "src/config/mod.rs",
        "mod paths;\nuse paths::{compile_includes, compile_excludes};\n",
    );
    repo.write(
        "src/config/paths.rs",
        "pub fn compile_includes() {}\npub fn compile_excludes() {}\n",
    );
    repo.write(
        "src/style/mod.rs",
        "use super::literal;\nmod string_literals; mod percent_array; mod symbol_array; mod word_array;\n",
    );
    repo.write(
        "src/style/string_literals.rs",
        "fn inspect_body(x: u8) -> u8 { x }\nfn to_string_literal(x: u8) -> u8 { x }\nfn escape_string(x: u8) -> u8 { x }\n",
    );
    repo.write(
        "src/literal.rs",
        "pub(crate) fn inspect_body(x: u8) -> u8 { x }\npub(crate) fn to_string_literal(x: u8) -> u8 { x }\npub(crate) fn escape_string(x: u8) -> u8 { x }\nfn first() -> String { format!(\"{}\", inspect_body(1)) }\nfn second() -> String { format!(\"{}\", inspect_body(1)) }\nfn third() -> String { format!(\"{}\", to_string_literal(1)) }\n",
    );
    repo.write(
        "src/style/percent_array.rs",
        "use super::literal::{escape_string};\nfn call() -> u8 { let out = escape_string(1); out }\n",
    );
    repo.write(
        "src/style/symbol_array.rs",
        "use super::literal::to_string_literal;\nfn call() -> u8 { to_string_literal(1) }\n",
    );
    repo.write(
        "src/style/word_array.rs",
        "use super::literal::to_string_literal;\nfn first() -> u8 { to_string_literal(1) }\nfn second() -> u8 { to_string_literal(1) }\n",
    );
    repo
}

fn change_private_helpers(repo: &TestRepo, suffix: &str) {
    repo.write(
        "src/style/string_literals.rs",
        format!(
            "fn inspect_body(x: u64) -> u64 {{ x }}\nfn to_string_literal(x: u64) -> u64 {{ x }}\nfn escape_string(x: u64) -> u64 {{ x }}\n{suffix}"
        ),
    );
}

fn helper_change(context: &serde_json::Value) -> &serde_json::Value {
    context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/style/string_literals.rs")
        .unwrap()
}

fn asserted_receiver_repo() -> TestRepo {
    let repo = private_helpers_repo();
    // Keep the assertion's ancestry ordinary. The unrelated module macro still
    // prevents the general owner resolver from hiding a broken leaf proof.
    repo.write("src/lib.rs", "mod config; mod literal; mod style;\n");
    repo.write(
        "src/style/mod.rs",
        "use super::literal;\nmod string_literals; mod percent_array; mod symbol_array; mod word_array;\nmacro_rules! marker { () => { #[allow(dead_code)] const MARKER: u8 = 0; } }\nmarker!();\n",
    );
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[[bin]]\nname = \"sample\"\npath = \"src/main.rs\"\n",
    );
    repo.write("src/main.rs", "fn main() {}\n");
    repo.create_dir_all("tests");
    repo.create_dir_all("crates/compat/src");
    repo.write(
        "tests/build_support.rs",
        "#[path = \"../crates/compat/src/build_support.rs\"]\nmod build_support;\n",
    );
    repo.write("crates/compat/src/build_support.rs", "pub fn build() {}\n");
    repo.write(
        "src/config/paths.rs",
        "pub fn compile_includes() {}\npub fn compile_excludes() {}\n#[derive(Clone, Debug, Default)]\nstruct Patterns;\nimpl Patterns { fn matches(&self, _: &str) -> bool { true } }\n#[cfg(test)]\nmod tests {\n    use super::Patterns;\n    #[test]\n    fn checks() {\n        let include = Patterns;\n        assert!(include.matches(\"file\"));\n        assert!(!include.matches(\"temporary\"));\n        assert!(include.matches(\"nowhere/deep/lib/a.rb\"));\n        assert!(include.matches(\"./nowhere/deep/lib/a.rb\"));\n    }\n}\n",
    );
    repo
}

#[test]
fn impact_rust_private_helpers_ignore_proven_assert_receivers() {
    let repo = asserted_receiver_repo();
    repo.init_git();
    repo.commit_all("initial");
    let refs_before = repo.run_json("refs", &["--name", "to_string_literal"]);
    let dead_before = repo.run_json("dead-code", &[]);
    change_private_helpers(&repo, "");

    let context = repo.run_json("context", &["--git"]);
    let change = helper_change(&context);
    assert_eq!(change["affected_symbols"].as_array().unwrap().len(), 3);
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket]
                .as_array()
                .is_none_or(|callers| callers.is_empty()),
            "{bucket}: {context}"
        );
    }
    let refs_after = repo.run_json("refs", &["--name", "to_string_literal"]);
    let references = |result: &serde_json::Value| {
        result["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|reference| reference["kind"] == "ref")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(references(&refs_before), references(&refs_after));
    assert_eq!(references(&refs_after).len(), 6);
    assert_eq!(dead_before, repo.run_json("dead-code", &[]));
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    repo.stage_all();
    let staged = repo.run_json("context", &["--git", "--staged"]);
    assert!(
        helper_change(&staged)["impacted_callers"]
            .as_array()
            .is_some_and(|callers| !callers.is_empty()),
        "staged: {staged}"
    );
}

#[test]
fn impact_rust_private_helpers_keep_unproven_assert_environments() {
    for (path, source) in [
        (
            "src/config/paths.rs",
            "macro_rules! assert { ($head:ident.$tail:ident($($args:tt)*)) => { $head!(\"extra.rs\"); }; }\nfn f() { assert!(include.matches(\"file\")); }\n",
        ),
        (
            "src/config/mod.rs",
            "macro_rules! assert { ($head:ident.$tail:ident($($args:tt)*)) => { $head!(\"extra.rs\"); }; }\nmod paths;\n",
        ),
        (
            "src/lib.rs",
            "macro_rules! assert { ($head:ident.$tail:ident($($args:tt)*)) => { $head!(\"extra.rs\"); }; }\nmod config; mod literal; mod style;\n",
        ),
        (
            "src/config/mod.rs",
            "use crate::other::load as assert;\nmod paths;\n",
        ),
        (
            "src/config/paths.rs",
            "use crate::other::*;\nfn f() { assert!(include.matches(\"file\")); }\n",
        ),
        (
            "src/lib.rs",
            "#[macro_use]\nmod other;\nmod config; mod literal; mod style;\n",
        ),
        (
            "src/config/paths.rs",
            "#[make_assert]\nstruct Generated;\nfn f() { assert!(include.matches(\"file\")); }\n",
        ),
        (
            "src/elsewhere.rs",
            "#[path = \"config/paths.rs\"]\nmod reused;\n",
        ),
        (
            "src/elsewhere.rs",
            "macro_rules! attach { ($attribute:ident) => { #[$attribute = \"config/paths.rs\"] mod reused; }; }\n",
        ),
        (
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[[bin]]\nname = \"alternate\"\npath = \"src/config/alternate.rs\"\n",
        ),
        ("src/config/paths.rs", "fn f() { load!(include.any); }\n"),
        (
            "src/config/paths.rs",
            "fn f() { assert!(include.matches(other!())); }\n",
        ),
        (
            "src/config/paths.rs",
            "fn f() { assert!(include!(\"extra.rs\")); }\n",
        ),
        (
            "src/elsewhere.rs",
            "#[macro_export]\nmacro_rules! println { () => { macro_rules! assert { ($head:ident.$method:ident()) => { $head!(\"extra.rs\"); }; } }; }\n",
        ),
    ] {
        let repo = asserted_receiver_repo();
        repo.write(path, source);
        repo.init_git();
        repo.commit_all("initial");
        change_private_helpers(&repo, "");
        let context = repo.run_json("context", &["--git"]);
        assert!(
            helper_change(&context)["impacted_callers"]
                .as_array()
                .is_some_and(|callers| callers
                    .iter()
                    .any(|caller| caller["path"] == "src/literal.rs")),
            "{path}: {source}\n{context}"
        );
    }
}

#[test]
fn impact_rust_private_helpers_assert_proof_keeps_descendant_callers() {
    let repo = asserted_receiver_repo();
    repo.create_dir_all("src/style/string_literals");
    repo.write(
        "src/style/string_literals/child.rs",
        "pub fn call() { super::inspect_body(1); }\n",
    );
    repo.write(
        "src/style/string_literals.rs",
        "fn inspect_body(x: u8) -> u8 { x }\nfn to_string_literal(x: u8) -> u8 { x }\nfn escape_string(x: u8) -> u8 { x }\nmod child;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    change_private_helpers(&repo, "mod child;\n");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        helper_change(&context)["impacted_callers"]
            .as_array()
            .is_some_and(|callers| callers.iter().any(|caller| {
                caller["path"] == "src/style/string_literals/child.rs"
                    && caller["symbols"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|name| name == "inspect_body")
            })),
        "{context}"
    );
}

#[test]
fn impact_rust_private_helpers_ignore_include_substrings_in_imports() {
    let repo = private_helpers_repo();
    repo.init_git();
    repo.commit_all("initial");
    let refs_before = repo.run_json("refs", &["--name", "to_string_literal"]);
    change_private_helpers(&repo, "");

    let context = repo.run_json("context", &["--git"]);
    let change = helper_change(&context);
    assert_eq!(change["affected_symbols"].as_array().unwrap().len(), 3);
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket]
                .as_array()
                .is_none_or(|callers| callers.is_empty()),
            "{bucket}: {context}"
        );
    }
    let refs_after = repo.run_json("refs", &["--name", "to_string_literal"]);
    let references = |result: &serde_json::Value| {
        result["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|reference| reference["kind"] == "ref")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(references(&refs_before), references(&refs_after));
    assert_eq!(references(&refs_after).len(), 6);

    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    repo.stage_all();
    let staged = repo.run_json("context", &["--git", "--staged"]);
    assert!(
        helper_change(&staged)["impacted_callers"]
            .as_array()
            .is_some_and(|callers| !callers.is_empty()),
        "staged の未証明な参照は保持する: {staged}"
    );
}

#[test]
fn impact_rust_private_helpers_keep_real_descendant_and_include_callers() {
    for (extra_source, suffix, caller_path) in [
        (
            "pub fn call() { super::inspect_body(1); }\n",
            "mod child;\n",
            "src/style/string_literals/child.rs",
        ),
        (
            "use std::include as load;\nload!(\"style/string_literals.rs\");\npub fn call() { inspect_body(1); }\n",
            "",
            "src/included.rs",
        ),
    ] {
        let repo = private_helpers_repo();
        repo.create_dir_all("src/style/string_literals");
        repo.write(caller_path, extra_source);
        if !suffix.is_empty() {
            repo.write(
                "src/style/string_literals.rs",
                format!(
                    "fn inspect_body(x: u8) -> u8 {{ x }}\nfn to_string_literal(x: u8) -> u8 {{ x }}\nfn escape_string(x: u8) -> u8 {{ x }}\n{suffix}"
                ),
            );
        } else {
            repo.write(
                "src/lib.rs",
                "mod config; mod literal; mod style; mod included;\nmacro_rules! marker { () => { const MARKER: u8 = 0; } }\nmarker!();\n",
            );
        }
        repo.init_git();
        repo.commit_all("initial");
        change_private_helpers(&repo, suffix);
        let context = repo.run_json("context", &["--git"]);
        let change = helper_change(&context);
        assert!(
            ["impacted_callers", "low_confidence_callers"]
                .iter()
                .any(|bucket| change[bucket]
                    .as_array()
                    .is_some_and(|callers| callers.iter().any(|caller| {
                        caller["path"] == caller_path
                            && caller["symbols"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|name| name == "inspect_body")
                    }))),
            "{caller_path}: {context}"
        );
        if !suffix.is_empty() {
            assert!(
                change["impacted_callers"]
                    .as_array()
                    .is_some_and(|callers| callers
                        .iter()
                        .any(|caller| caller["path"] == caller_path))
            );
            let output = cargo_bin()
                .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{output:?}");
        }
    }
}
