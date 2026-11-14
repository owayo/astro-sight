//! Rust のローカル束縛と変更関数の caller を区別する。

use super::support::*;

fn distinct_function_workspace(update_caller: bool, print: bool, scenario: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.write("Cargo.toml", "[workspace]\nmembers = [\"crates/core-lib\", \"crates/fetch-lib\", \"crates/sched-lib\", \"crates/app\"]\nresolver = \"3\"\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    for (package, dependencies) in [
        ("core-lib", ""),
        ("fetch-lib", "core-lib = { path = '../core-lib' }"),
        ("sched-lib", "core-lib = { path = '../core-lib' }"),
        (
            "app",
            "core-lib = { path = '../core-lib' }\nfetch-lib = { path = '../fetch-lib' }\nsched-lib = { path = '../sched-lib' }",
        ),
    ] {
        repo.create_dir_all(format!("crates/{package}/src"));
        repo.write(format!("crates/{package}/Cargo.toml"), format!("[package]\nname = '{package}'\nversion.workspace = true\nedition.workspace = true\n[dependencies]\n{dependencies}\n"));
    }
    for (path, source) in [
        (
            "core-lib/src/lib.rs",
            "pub mod catalog; pub mod image_ref; pub mod paths;\n",
        ),
        (
            "core-lib/src/catalog.rs",
            "pub fn resolve(names: &[&str]) -> Vec<String> { names.iter().map(|name| name.to_string()).collect() }\n",
        ),
        (
            "core-lib/src/paths.rs",
            "pub struct AppPaths { pub root: String }\nimpl AppPaths { pub fn resolve() -> Self { Self { root: String::new() } } }\n",
        ),
        (
            "core-lib/src/image_ref.rs",
            "pub fn expand(html: &str, mut resolve: impl FnMut(&str) -> Option<String>) -> String { match resolve(html) { Some(url) => url, None => html.to_string() } }\npub fn expand_local(html: &str) -> String { let resolve = |text: &str| Some(text.to_uppercase()); resolve(html).unwrap_or_default() }\n",
        ),
        (
            "fetch-lib/src/lib.rs",
            "mod engine; pub use engine::absolute;\n",
        ),
        (
            "fetch-lib/src/engine.rs",
            "use core_lib::image_ref::expand;\npub fn absolute(raw: &str) -> Option<String> { let raw = expand(raw, |_| None); Some(raw).and_then(|raw| resolve(&raw, \"https://example.com\")) }\nfn resolve(raw: &str, base: &str) -> Option<String> { Some(format!(\"{base}/{raw}\")) }\n",
        ),
        (
            "sched-lib/src/lib.rs",
            "mod facts; mod resolve; pub use facts::local;\n",
        ),
        (
            "sched-lib/src/resolve.rs",
            "pub struct Local { pub name: String }\n",
        ),
        (
            "sched-lib/src/facts.rs",
            "use core_lib::paths::AppPaths;\nuse crate::resolve::Local;\npub fn local() -> Local { let root = AppPaths { root: String::new() }.root; crate::resolve::Local { name: root } }\n",
        ),
        (
            "app/src/services.rs",
            "pub fn sources() -> Vec<String> { core_lib::catalog::resolve(&[\"a\", \"b\"]) }\n",
        ),
    ] {
        repo.write(format!("crates/{path}"), source);
    }
    let output = if print {
        "println!(\"{} {:?} {:?}\", paths.root, fetch_lib::absolute(\"a\"), services::sources());"
    } else {
        "let _ = (paths.root, fetch_lib::absolute(\"a\"), services::sources());"
    };
    repo.write("crates/app/src/main.rs", format!("mod services;\nuse core_lib::paths::AppPaths;\nfn main() {{ let paths = AppPaths::resolve(); let _ = sched_lib::local(); {output} }}\n"));
    match scenario {
        "module alias" => repo.write("crates/app/src/main.rs", "use core_lib::catalog as AppPaths;\nfn main() { let _ = AppPaths::resolve(&[\"a\"]); }\n"),
        "local crate name" => repo.write("crates/app/src/main.rs", "mod core_lib { pub mod paths { pub use ::core_lib::catalog as AppPaths; } }\nuse core_lib::paths::AppPaths;\nfn main() { let _ = AppPaths::resolve(&[\"a\"]); }\n"),
        "type alias" => repo.write("crates/core-lib/src/paths.rs", "pub type AppPaths = String;\n"),
        "conditional type" => repo.write("crates/core-lib/src/paths.rs", "#[cfg(feature = \"enabled\")] pub struct AppPaths;\n"),
        "macro shadow" => repo.write("crates/app/src/main.rs", "use core_lib::paths::AppPaths;\nmacro_rules! println { ($($arg:tt)*) => {}; }\nfn main() { let _ = AppPaths::resolve(); println!(\"ready\"); }\n"),
        "compiler config" => {
            repo.create_dir_all(".cargo");
            repo.write(".cargo/config.toml", "[build]\nrustflags = [\"--extern\", \"core_lib=other.rlib\"]\n");
        }
        "renamed" => {
            repo.write("crates/app/Cargo.toml", "[package]\nname = 'app'\nversion.workspace = true\nedition.workspace = true\n[dependencies]\nshared = { package = 'core-lib', path = '../core-lib' }\nfetch-lib = { path = '../fetch-lib' }\nsched-lib = { path = '../sched-lib' }\n");
            repo.write("crates/app/src/main.rs", format!("mod services;\nuse shared::paths::AppPaths;\nfn main() {{ let paths = AppPaths::resolve(); let _ = sched_lib::local(); {output} }}\n"));
            repo.write("crates/app/src/services.rs", "pub fn sources() -> Vec<String> { shared::catalog::resolve(&[\"a\", \"b\"]) }\n");
        }
        "plain" => {}
        _ => panic!("unknown scenario: {scenario}"),
    }
    repo.init_git();
    repo.commit_all("initial");
    repo.write("crates/core-lib/src/catalog.rs", "pub fn resolve(names: &[&str], upper: bool) -> Vec<String> { names.iter().map(|name| if upper { name.to_uppercase() } else { name.to_string() }).collect() }\n");
    if update_caller {
        let crate_name = if scenario == "renamed" {
            "shared"
        } else {
            "core_lib"
        };
        repo.write("crates/app/src/services.rs", format!("pub fn sources() -> Vec<String> {{ {crate_name}::catalog::resolve(&[\"a\", \"b\"], false) }}\n"));
    }
    repo
}

#[test]
fn impact_rust_distinguishes_workspace_function_uses() {
    for print in [false, true] {
        for update in [false, true] {
            let repo = distinct_function_workspace(update, print, "plain");
            let context = repo.run_json("context", &["--git"]);
            let callers: Vec<_> = context["changes"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|change| change["impacted_callers"].as_array().into_iter().flatten())
                .map(|caller| caller["path"].as_str().unwrap())
                .collect();
            assert_eq!(
                callers,
                vec!["crates/app/src/services.rs"],
                "{print} / {update}: {context}"
            );
            let output = cargo_bin()
                .args(["impact", "--git", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(i32::from(!update)), "{context}");
            assert!(
                context["changes"].as_array().unwrap().iter().all(|change| {
                    change["low_confidence_callers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .all(|caller| caller["path"] != "crates/core-lib/src/image_ref.rs")
                }),
                "{context}"
            );
            if update && !print {
                let references = repo.run_json("refs", &["--name", "resolve"]);
                for path in [
                    "crates/app/src/main.rs",
                    "crates/core-lib/src/image_ref.rs",
                    "crates/sched-lib/src/facts.rs",
                ] {
                    assert!(
                        references["refs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|reference| reference["path"] == path),
                        "{references}"
                    );
                }
            }
        }
    }
}

#[test]
fn impact_rust_external_associated_types_require_binding_proof() {
    for scenario in [
        "module alias",
        "local crate name",
        "type alias",
        "conditional type",
        "macro shadow",
        "compiler config",
    ] {
        let repo = distinct_function_workspace(true, true, scenario);
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"].as_array().unwrap().iter().any(|change| {
                change["impacted_callers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|caller| caller["path"] == "crates/app/src/main.rs")
            }),
            "{scenario}: {context}"
        );
    }
    let repo = distinct_function_workspace(true, true, "renamed");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|change| change["impacted_callers"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|caller| caller["path"] != "crates/app/src/main.rs")),
        "{context}"
    );
}

#[test]
fn impact_rust_associated_paths_ignore_unrelated_file_syntax() {
    for (declaration, extra, body, imported) in [
        (
            "pub struct Catalog;",
            "",
            "let sizes = vec![1usize, 2]; sizes.len() +",
            false,
        ),
        ("#[derive(Debug)]\npub struct Catalog;", "", "", false),
        (
            "pub struct Catalog;",
            "mod unrelated { pub struct Other; }",
            "",
            false,
        ),
        (
            "pub struct Catalog;",
            "#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert!(true); } }",
            "",
            false,
        ),
        (
            "pub struct Catalog;",
            "pub fn first<T: Copy>(items: &[T]) -> Option<T> { items.first().copied() }",
            "",
            false,
        ),
        ("pub struct Catalog;", "", "", true),
        (
            "pub struct Catalog;",
            "#[cfg(test)] mod tests { use super::*; }",
            "",
            true,
        ),
    ] {
        for actual in [false, true] {
            let repo = TestRepo::new();
            repo.create_dir_all("src");
            repo.write(
                "Cargo.toml",
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            );
            repo.write(
                "src/lib.rs",
                "pub mod catalog;\npub mod report;\npub mod text;\n",
            );
            repo.write(
                "src/text.rs",
                "pub(crate) fn parse(input: &str) -> usize { input.len() }\n",
            );
            let total = format!("pub fn total() -> usize {{ {body} Catalog::parse(\"x\") }}\n");
            repo.write("src/catalog.rs", format!("{declaration}\nimpl Catalog {{ pub fn parse(input: &str) -> usize {{ input.len() * 2 }} }}\n{extra}\n{}", if imported { "" } else { &total }));
            repo.write(
                "src/report.rs",
                format!(
                    "{}{}",
                    if imported {
                        format!("use crate::catalog::Catalog;\n{total}")
                    } else {
                        String::new()
                    },
                    if actual {
                        "pub fn actual() -> usize { crate::text::parse(\"x\") }\n"
                    } else {
                        ""
                    }
                ),
            );
            repo.init_git();
            repo.commit_all("initial");
            repo.write(
                "src/text.rs",
                "pub(crate) fn parse(input: &str, base: usize) -> usize { input.len() + base }\n",
            );
            let context = repo.run_json("context", &["--git"]);
            let callers = context["changes"][0]["impacted_callers"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            assert_eq!(
                callers.len(),
                usize::from(actual),
                "{declaration} / {extra} / {imported}: {context}"
            );
            let output = cargo_bin()
                .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(i32::from(actual)), "{context}");
        }
    }
}

#[test]
fn impact_rust_associated_paths_keep_unknown_or_shadowed_type_bindings() {
    for consumer in [
        "pub struct Catalog; make_items!(); pub fn total() -> usize { Catalog::parse(\"x\") }",
        "pub struct Catalog; macro_rules! make_items { () => {} } pub fn total() -> usize { Catalog::parse(\"x\") }",
        "use crate::text as Catalog; pub fn total() -> usize { Catalog::parse(\"x\") }",
        "pub struct Catalog; pub fn total<Catalog>() -> usize { Catalog::parse(\"x\") }",
        "pub struct Catalog; pub fn total<r#Catalog>() -> usize { Catalog::parse(\"x\") }",
        "pub struct Catalog; pub trait Other<Catalog> { fn total() -> usize { Catalog::parse(\"x\") } }",
        "pub struct Katalog; pub fn total<Katalog>() -> usize { Katalog::parse(\"x\") }",
        "pub struct Catalog; pub fn total() -> usize { use crate::text as Catalog; Catalog::parse(\"x\") }",
        "pub struct Catalog; pub fn total() -> usize { type Catalog = crate::text::Catalog; Catalog::parse(\"x\") }",
        "pub struct Catalog; pub fn total() -> usize { mod r#Catalog { pub use crate::text::parse; } Catalog::parse(\"x\") }",
        "use custom::Debug; #[derive(Debug)] pub struct Catalog; pub fn total() -> usize { Catalog::parse(\"x\") }",
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "mod text;\nmod consumer;\n");
        repo.write(
            "src/text.rs",
            "pub(crate) fn parse(input: &str) -> usize { input.len() }\n",
        );
        repo.write("src/consumer.rs", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/text.rs",
            "pub(crate) fn parse(input: &str, base: usize) -> usize { input.len() + base }\n",
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "src/consumer.rs"),
            "{consumer}: {context}"
        );
    }
}

fn rust_scope_repo(consumer: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.write("api.rs", "pub fn comments() -> usize { 1 }\n");
    repo.write("consumer.rs", consumer);
    repo.init_git();
    repo.commit_all("initial");
    repo.write("api.rs", "pub fn comments() -> u32 { 1 }\n");
    repo
}

#[test]
fn impact_rust_scope_inner_let_with_child_test_glob_preserves_real_calls() {
    for test_import in ["use super::*;", "use super::first_gap;"] {
        for actual_call in ["", "crate::jobs::start", "start"] {
            let repo = TestRepo::new();
            repo.create_dir_all("src");
            repo.write(
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            );
            repo.write("src/lib.rs", "mod jobs;\nmod runs;\n");
            repo.write("src/jobs.rs", "pub fn start(n: u32) -> u32 {\n n + 1\n}\n");
            let caller = if actual_call.is_empty() {
                String::new()
            } else {
                format!(
                    "{}pub fn actual() -> u32 {{ {actual_call}(1) }}\n",
                    if actual_call == "start" {
                        "use crate::jobs::*;\n"
                    } else {
                        ""
                    }
                )
            };
            repo.write(
                "src/runs.rs",
                format!(
                    r#"pub fn first_gap(values: &[u32]) -> Option<usize> {{
    let mut cursor = 0;
    while let Some(offset) = values[cursor..].iter().position(|v| *v == 0) {{
        let start = cursor + offset;
        if start > 2 {{
            return Some(start);
        }}
        cursor = start + 1;
    }}
    None
}}

#[cfg(test)]
mod tests {{
    {test_import}
    #[test]
    fn finds_gap() {{
        assert_eq!(first_gap(&[1, 1, 1, 0]), Some(3));
    }}
}}
{caller}"#
                ),
            );
            repo.init_git();
            repo.commit_all("initial");
            repo.write("src/jobs.rs", "pub fn start(n: u64) -> u64 {\n n + 1\n}\n");

            let context = repo.run_json("context", &["--git"]);
            let callers = context["changes"][0]["impacted_callers"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            assert!(
                callers.iter().all(|caller| {
                    caller["path"] != "src/runs.rs"
                        || !matches!(caller["line"].as_u64().unwrap(), 3 | 4 | 5 | 7)
                }),
                "{test_import}, {actual_call}: {context}"
            );
            assert_eq!(
                callers.iter().any(|caller| caller["path"] == "src/runs.rs"),
                !actual_call.is_empty(),
                "{test_import}, {actual_call}: {context}"
            );
            let output = cargo_bin()
                .args([
                    "review",
                    "--dir",
                    repo.root().to_str().unwrap(),
                    "--git",
                    "--hook",
                ])
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(i32::from(!actual_call.is_empty())),
                "{test_import}, {actual_call}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn impact_rust_scope_review_keeps_calls_through_same_named_function_aliases() {
    for (consumer, call_line) in [
        (
            "pub fn total() -> usize {\n let comments;\n (comments,) = (crate::api::comments,);\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let r#comments;\n r#comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments;\n let capture = || { comments = crate::api::comments; };\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments;\n comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let mut comments = || 1usize;\n comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total(mut comments: fn() -> usize) -> usize {\n comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
        (
            "use crate::api::comments;\npub fn total() -> usize {\n let comments;\n comments = crate::api::r#comments;\n comments()\n}\n",
            4,
        ),
        (
            "pub fn total() -> usize {\n let comments;\n comments = identity!(crate::api::comments);\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
        (
            "use crate::api::comments;\npub fn total() -> usize {\n let comments = comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments = 1;\n let comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total(comments: usize) -> usize {\n let comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
        (
            "pub fn total() -> usize {\n let comments = 1;\n #[cfg(feature=\"x\")] let comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments = identity!(crate::api::comments);\n comments()\n}\n",
            2,
        ),
        (
            "pub fn total() -> usize {\n let comments = || crate::api::comments();\n comments()\n}\n",
            2,
        ),
        (
            "pub fn total() -> usize {\n let r#comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
    ] {
        let repo = rust_scope_repo(consumer);
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{consumer}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == call_line),
            "{consumer}: {context}"
        );
    }
}

#[test]
fn impact_rust_scope_review2_keeps_body_only_changes_when_same_named_items_are_added() {
    let before = "pub fn comments() -> usize {\n 1\n}\npub struct B;\n\n\n\n\n\n\n\n\n\n\n\n\n";
    let mut failures = Vec::new();
    for added in [
        "impl B { pub fn comments(&self) -> usize { 3 } }\n",
        "pub mod legacy { pub const comments: usize = 1; }\n",
    ] {
        let repo = TestRepo::new();
        repo.write("api.rs", before);
        repo.write(
            "consumer.rs",
            "pub fn f() -> usize { crate::api::comments() }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("api.rs", format!("{}{added}", before.replace(" 1", " 2")));
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        let context = repo.run_json("context", &["--git"]);
        let blocking = context["changes"].as_array().unwrap().iter().any(|change| {
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| !callers.is_empty())
        });
        if !output.status.success() || blocking {
            failures.push(format!(
                "{added}: exit={:?} {context}",
                output.status.code()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn impact_rust_scope_review_keeps_old_constant_patterns_when_kind_becomes_function() {
    for (consumer, expected_line) in [
        (
            "use crate::api::comments;\npub fn f(v: u8) -> u8 {\n let comments = v else { return 0; };\n comments\n}\n",
            3,
        ),
        (
            "pub fn f(v: u8) -> u8 {\n use crate::api::comments; let comments = v else { return 0; }; comments\n}\n",
            1,
        ),
    ] {
        let repo = TestRepo::new();
        repo.write(
            "api.rs",
            "#[allow(non_upper_case_globals)]\npub const comments: u8 = 1;\n",
        );
        repo.write("consumer.rs", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("api.rs", "pub fn comments() -> u8 { 1 }\n");
        let context = repo.run_json("context", &["--git"]);
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{context}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == expected_line),
            "{context}"
        );
    }
}

#[test]
fn impact_rust_scope_keeps_other_source_and_mixed_kind_changes() {
    for (before, after, other_source) in [
        (
            "pub fn comments() -> usize { 1 }\n",
            "pub fn comments() -> u32 { 1 }\n",
            true,
        ),
        (
            "pub fn comments() -> usize { 1 }\npub mod values {\n pub const comments: usize = 1;\n}\n",
            "pub fn comments() -> u32 { 1 }\npub mod values {\n pub const comments: u32 = 1;\n}\n",
            false,
        ),
        (
            "pub fn comments() -> usize { 1 }\n",
            "pub const comments: usize = 1;\n",
            false,
        ),
        (
            "pub struct comments;\n",
            "pub struct comments { pub value: u8 }\n",
            false,
        ),
    ] {
        let repo = TestRepo::new();
        repo.write("api.rs", before);
        if other_source {
            repo.write("values.rs", "pub const comments: usize = 1;\n");
        }
        repo.write("consumer.rs", "use crate::api::*;\nuse crate::values::*;\nfn f() {\n let comments = 1;\n comments;\n}\n");
        repo.init_git();
        repo.commit_all("initial");
        repo.write("api.rs", after);
        if other_source {
            repo.write("values.rs", "pub const comments: u32 = 1;\n");
        }
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{before} → {after}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = repo.run_json("context", &["--git"]);
        let changes = context["changes"].as_array().unwrap();
        let source = if other_source { "values.rs" } else { "api.rs" };
        let change = changes
            .iter()
            .find(|change| change["path"] == source)
            .unwrap();
        assert!(
            change["affected_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["name"] == "comments" && symbol["kind"] != "function"),
            "{context}"
        );
        assert!(
            change["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == 4),
            "{context}"
        );
        if other_source {
            let functions = changes
                .iter()
                .find(|change| change["path"] == "api.rs")
                .unwrap();
            assert!(
                functions["impacted_callers"]
                    .as_array()
                    .is_none_or(Vec::is_empty),
                "{context}"
            );
        }
        let mut expected = None;
        for workers in ["1", "3", "1"] {
            let output = cargo_bin()
                .args(["context", "--dir", repo.root().to_str().unwrap(), "--git"])
                .env("ASTRO_SIGHT_IMPACT_WORKERS", workers)
                .output()
                .unwrap();
            assert!(output.status.success());
            if let Some(expected) = &expected {
                assert_eq!(&output.stdout, expected, "workers={workers}");
            } else {
                let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(actual, context);
                expected = Some(output.stdout);
            }
        }
    }
}

#[test]
fn impact_rust_scope_preserves_public_refs_and_dead_code_counts() {
    let repo = rust_scope_repo(
        "use crate::api::comments;\nfn f() {\n let comments = vec![1];\n comments.len();\n let capture = || comments;\n}\n",
    );
    let refs = repo.run_json("refs", &["--name", "comments"]);
    assert_eq!(refs["refs"].as_array().unwrap().len(), 5, "{refs}");
    let dead = repo.run_json("dead-code", &[]);
    assert!(
        !dead["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "comments"),
        "{dead}"
    );
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(context["changes"].as_array().unwrap().len(), 1);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{context}"
    );
}

#[test]
fn impact_rust_scope_excludes_local_patterns_parameters_and_captures() {
    for import in ["", "use crate::api::comments;\n", "use crate::api::*;\n"] {
        for source in [
            "fn f() { let comments = vec![1usize]; comments.len(); }",
            "fn f(comments: Vec<u8>) { comments.len(); }",
            "fn f((comments, _): (u8, u8)) { comments; }",
            "fn f() { let (comments, _) = pair; comments; }",
            "fn f() { let Data { comments, .. } = value; comments; }",
            "fn f() { let Data { x: comments } = value; comments; }",
            "fn f() { let Some(comments) = value else { return; }; comments; }",
            "fn f() { let comments = 1; { comments; } let capture = || comments; }",
            "fn f() { let comments = 1; let capture = async move { comments }; }",
            "fn f() { let comments = 1; for value in values { consume(comments); } }",
            "mod tests { use super::*; fn f() { let comments = 1; comments; } }",
        ] {
            let repo = rust_scope_repo(&format!("{import}{source}\n"));
            let output = cargo_bin()
                .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{import}{source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let context = repo.run_json("context", &["--git"]);
            let change = &context["changes"][0];
            assert_eq!(context["changes"].as_array().unwrap().len(), 1);
            for bucket in [
                "impacted_callers",
                "low_confidence_callers",
                "informational_callers",
            ] {
                assert!(
                    !change[bucket]
                        .as_array()
                        .is_some_and(|callers| callers.iter().any(|caller| caller["path"]
                            == "consumer.rs"
                            && !(bucket == "informational_callers"
                                && !import.is_empty()
                                && caller["line"] == 0))),
                    "{source}: {context}"
                );
            }
        }
    }
}

#[test]
fn impact_rust_scope_keeps_actual_calls_and_unproven_positions() {
    for (source, expected_line) in [
        (
            "fn f() {\n comments();\n let comments = vec![1];\n comments.len();\n}",
            2,
        ),
        (
            "fn f() {\n let comments = comments();\n comments.len();\n}",
            2,
        ),
        ("fn f() {\n let comments = || comments();\n}", 2),
        (
            "fn f() {\n let Some(comments) = value else { comments(); return; };\n}",
            2,
        ),
        ("fn f() {\n { let comments = 1; }\n comments();\n}", 3),
        (
            "fn f() {\n let comments = 1;\n fn inner() { comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n let x = const { comments() };\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n { use crate::api::comments; comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n { use crate::api::*; comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n #[cfg(feature=\"x\")] let comments = 1;\n comments();\n}",
            3,
        ),
        (
            "fn f(#[cfg(feature=\"x\")] comments: Vec<u8>) {\n comments();\n}",
            2,
        ),
        (
            "fn f() {\n let comments = 1;\n create_binding!();\n comments();\n}",
            4,
        ),
        (
            "fn f() {\n let comments = 1;\n crate::api::comments();\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n if let Some(x) = value { comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n while let Some(x) = value { comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n match value { _ => comments() };\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n comments();\n let broken = ;\n}",
            3,
        ),
    ] {
        let repo = rust_scope_repo(&format!("use crate::api::comments;\n{source}\n"));
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == expected_line),
            "{source}: {context}"
        );
    }
}

#[test]
fn impact_rust_scope_original_local_declaration_is_not_a_caller() {
    let repo = TestRepo::new();
    repo.write("api.rs", "pub fn comments() -> usize { 1 }\n");
    repo.write(
        "consumer.rs",
        "pub fn group() -> usize {\n    let comments = vec![1usize];\n    comments.len()\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("api.rs", "pub fn comments() -> u32 { 1 }\n");
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(context["changes"].as_array().unwrap().len(), 1);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{context}"
    );
    let review = repo.run_json("review", &["--git"]);
    assert!(
        review["impact"]["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{review}"
    );
    assert!(
        !review["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{review}"
    );
}

#[test]
fn impact_rust_scope_outer_string_in_for_with_child_test_glob_preserves_real_calls() {
    for actual in [false, true] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "mod jobs;\nmod render;\n");
        repo.write("src/jobs.rs", "pub fn run(n: u32) -> u32 { n + 1 }\n");
        repo.write(
            "src/render.rs",
            format!(
                r#"pub fn collect(values: &[char]) -> String {{
    let mut run = String::new();
    for (index, value) in values.iter().enumerate() {{
        if index > 0 && !run.is_empty() {{
            let previous = std::mem::take(&mut run);
        }}
        run.push(*value);
    }}
    run
}}
#[cfg(test)]
mod tests {{ use super::*; }}
{}"#,
                if actual {
                    "pub fn actual() -> u32 { crate::jobs::run(1) }\n"
                } else {
                    ""
                }
            ),
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/jobs.rs", "pub fn run(n: u64) -> u64 { n + 1 }\n");
        let context = repo.run_json("context", &["--git"]);
        let change = &context["changes"][0];
        for bucket in [
            "impacted_callers",
            "low_confidence_callers",
            "informational_callers",
        ] {
            assert!(
                change[bucket].as_array().is_none_or(|callers| {
                    callers.iter().all(|caller| {
                        caller["path"] != "src/render.rs" || caller["line"].as_u64().unwrap() > 9
                    })
                }),
                "actual={actual}, {bucket}: {context}"
            );
        }
        let callers = change["impacted_callers"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        assert_eq!(callers.len(), usize::from(actual), "{context}");
        let refs = repo.run_json("refs", &["--name", "run"]);
        assert_eq!(
            refs["refs"].as_array().unwrap().len(),
            6 + usize::from(actual)
        );
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(i32::from(actual)), "{context}");
    }
}

#[test]
fn impact_rust_scope_macro_arguments_exclude_only_proven_local_values() {
    for source in [
        "pub fn f() { let comments = 1; println!(\"{}\", comments); }",
        "pub fn f(comments: u8) { eprintln!(\"{}\", comments); }",
        "pub fn f() { let r#comments = 1; print!(\"{}\", r#comments); }",
        "pub fn f() { let comments = 1; let text = format!(\"{}\", comments); }",
        "pub fn f() { let comments = 1; dbg!(comments); }",
        "pub fn f() { let comments = 1; println!(\"{}\", /* before */ comments /* after */,); }",
    ] {
        let repo = rust_scope_repo(source);
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{source}: {context}"
        );
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let refs = repo.run_json("refs", &["--name", "comments"]);
        assert_eq!(
            refs["refs"].as_array().unwrap().len(),
            3,
            "{source}: {refs}"
        );
        let dead = repo.run_json("dead-code", &[]);
        assert!(
            !dead["dead_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["name"] == "comments"),
            "{dead}"
        );

        repo.write(
            "consumer.rs",
            format!(
                "{source}\npub fn actual() {{ println!(\"{{}}\", crate::api::comments()); }}\n"
            ),
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == 1),
            "{context}"
        );
    }
}

#[test]
fn impact_rust_scope_macro_arguments_keep_unproven_syntax_and_macro_origins() {
    for (source, extra) in [
        ("pub fn f() { println!(\"{}\", comments()); }", ""),
        (
            "pub fn f() { println!(\"{}\", comments); let comments = 1; }",
            "",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments); let broken = ; }",
            "",
        ),
        ("pub fn f() { let comments = 1; custom!(comments); }", ""),
        (
            "pub fn f() { let comments = 1; outer!(println!(\"{}\", comments)); }",
            "",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "gen!();",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "macro_rules! shadow { () => { macro_rules! println { ($f:expr, $i:ident) => { crate::api::$i() } } } } shadow!();",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", object.comments); }",
            "",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", crate::api::comments()); }",
            "",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", (comments)); }",
            "",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments()); }",
            "",
        ),
        (
            "pub fn f() { let comments = crate::api::comments; println!(\"{:?}\", comments); }",
            "",
        ),
        (
            "pub fn f() { let comments = 1; unknown!(); println!(\"{}\", comments); }",
            "",
        ),
        (
            "macro_rules! println { ($x:expr, $i:ident) => { crate::api::$i() }; } pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "",
        ),
        (
            "use external::other as println; pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "",
        ),
        (
            "use external::*; pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "#[macro_export] macro_rules! println { ($x:expr, $i:ident) => { crate::api::$i() }; }",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "use external::other as println;",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "macro_rules! r#println { ($x:expr, $i:ident) => { crate::api::$i() }; }",
        ),
        (
            "pub fn f() { let comments = 1; println!(\"{}\", comments); }",
            "use external::Custom as Debug; #[derive(Debug)] struct Value;",
        ),
    ] {
        let repo = rust_scope_repo(source);
        if !extra.is_empty() {
            repo.write("macros.rs", extra);
        }
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs"),
            "{source} / {extra}: {context}"
        );
    }
}
