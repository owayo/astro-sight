//! Rust の inherent メソッドの変更で、別の型のパスの呼び出しを呼び出し元にしない (Issue #470)。

use std::collections::BTreeSet;

use super::support::*;

const LIB: &str = "pub mod config;\npub mod server;\npub struct Engine;\nimpl Engine {\n    pub fn new() -> Self {\n        Engine\n    }\n    pub fn handle(&self, id: u32) -> u32 {\n        id\n    }\n}\n";
const CONFIG: &str = "#[derive(Clone, Default)]\npub struct Config {\n    pub learning: bool,\n}\npub fn new() -> Config {\n    Config::default()\n}\n";
/// Issue の最小の再現の `server.rs`。`Box::new(config::new())` (prelude の型とモジュールの
/// 自由関数) を足してある。
const SERVER: &str = "use crate::{Engine, config};\nmod admin;\nuse std::sync::{Arc, Mutex};\npub fn serve() -> std::io::Result<String> {\n    let engine = Engine::new();\n    let _ = engine.handle(1);\n    let admin = Arc::new(admin::Administration::new(config::Config::default()));\n    let shared = Arc::new(Mutex::new(0));\n    let boxed = Box::new(config::new());\n    let worker = std::thread::Builder::new().spawn(move || *shared.lock().unwrap());\n    let _ = (worker, boxed);\n    admin.handle()\n}\n";
const ADMIN_OLD: &str = "use super::*;\npub(super) struct Administration {\n    settings: Mutex<config::Config>,\n}\nimpl Administration {\n    pub(super) fn new(settings: config::Config) -> Self {\n        Self {\n            settings: Mutex::new(settings),\n        }\n    }\n    pub(super) fn handle(&self) -> std::io::Result<String> {\n        let current = self.settings.lock().unwrap();\n        Ok(format!(\"{}\", current.learning && ipc::ping()))\n    }\n}\n";
const ADMIN_NEW: &str = "use super::*;\npub(super) struct Administration {\n    settings: Mutex<Option<config::Config>>,\n}\nimpl Administration {\n    pub(super) fn new(settings: Option<config::Config>) -> Self {\n        Self {\n            settings: Mutex::new(settings),\n        }\n    }\n    pub(super) fn handle(&self) -> std::io::Result<String> {\n        let current = self.settings.lock().unwrap();\n        Ok(format!(\"{}\", current.is_some() && ipc::ping()))\n    }\n}\n";

/// 真の呼び出し元の行。
const ADMIN_CALL: &str =
    "let admin = Arc::new(admin::Administration::new(config::Config::default()));";

#[test]
fn rust_associated_function_distinguishes_same_spelled_local_types() {
    let producer =
        "struct Names;\nimpl Names {\n    fn collect() -> Self { Self }\n}\nmod child;\n";
    let local = r#"struct Names;
impl Names {
    fn collect() -> Self { Self }
}
pub fn run() { let _: Names = Names::collect(); }
fn pointer() { let _: fn() -> Names = Names::collect; }
fn anchored() { let _ = self::Names::collect(); }
"#;
    for true_calls in [false, true] {
        let repo = TestRepo::new();
        repo.create_dir_all("src/producer");
        repo.write(
            "Cargo.toml",
            "[package]\nname='owners'\nversion='0.1.0'\nedition='2024'\n",
        );
        let producer = if true_calls {
            repo.write(
                "src/lib.rs",
                "mod producer;\nmod consumer;\nmod switched;\n",
            );
            repo.write("src/switched.rs", "use crate::producer::Names as Imported;\ntype Names = Imported;\ntrait Factory { fn collect() -> u8 { 1 } }\nimpl Factory for Names {}\nfn value() -> u8 { Names::collect() }\n");
            producer.replacen("struct Names", "pub(super) struct Names", 1)
        } else {
            repo.write("src/lib.rs", "mod producer;\nmod consumer;\n");
            producer.to_owned()
        };
        repo.write("src/producer.rs", &producer);
        repo.write("src/consumer.rs", local);
        let child = if true_calls {
            format!(
                "{local}fn both() {{ let _ = (Names::collect(), super::Names::collect()); }}\nuse super::Names as Actual;\nfn imported() {{ let _ = Actual::collect(); }}\nfn rebound() {{ type Names = super::Names; let _ = Names::collect(); }}\n"
            )
        } else {
            String::new()
        };
        repo.write("src/producer/child.rs", child);
        repo.init_git();
        repo.commit_all("base");
        repo.write(
            "src/producer.rs",
            producer.replace("fn collect", "pub(super) fn collect"),
        );
        for staged in [false, true] {
            let args = if staged {
                repo.stage_all();
                &["--git", "--staged"][..]
            } else {
                &["--git"][..]
            };
            let context = repo.run_json("context", args);
            let changes = context["changes"][0]["impacted_callers"].as_array();
            let actual: BTreeSet<_> = changes
                .into_iter()
                .flatten()
                .map(|r| (r["path"].as_str().unwrap(), r["line"].as_u64().unwrap()))
                .collect();
            let expected = if true_calls {
                BTreeSet::from([
                    ("src/producer/child.rs", 7),
                    ("src/producer/child.rs", 9),
                    ("src/producer/child.rs", 10),
                    ("src/switched.rs", 4),
                ])
            } else {
                BTreeSet::new()
            };
            assert_eq!(actual, expected, "staged={staged}: {context}");
        }
    }
}

#[test]
fn rust_widened_inherent_function_keeps_implicit_and_projected_trait_calls() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='dispatch'\nversion='0.1.0'\nedition='2024'\n",
    );
    let library = "mod producer; mod consumer;\n";
    repo.write("src/lib.rs", library);
    repo.write("README.md", "Dispatch\n");
    let producer = "pub(super) struct Names;\nimpl Names {\n    fn collect() -> u8 { 2 }\n}\n";
    repo.write("src/producer.rs", producer);
    repo.write(
        "src/consumer.rs",
        r#"use crate::producer::Names;
pub trait Gather { type Item; fn collect() -> u8; }
impl Gather for Names { type Item = Names; fn collect() -> u8 { 1 } }
fn fixed() -> u8 { <Names as Gather>::collect() }
fn pointer() -> fn() -> u8 { <Names as Gather>::collect }
fn inferred() -> u8 { Names::collect() }
fn bracketed() -> u8 { <Names>::collect() }
fn projected() -> u8 { <Names as Gather>::Item::collect() }
fn both() -> u8 { <Names as Gather>::collect() + Names::collect() }
macro_rules! implicit { (<$owner:ty as $t:path>::$method:ident) => { <$owner>::$method() }; }
fn rewritten() -> u8 { implicit!(<Names as Gather>::collect) }
fn annotated() -> u8 { </* owner */ Names as Gather>::collect() }
"#,
    );
    repo.init_git();
    repo.commit_all("base");
    repo.write(
        "src/producer.rs",
        producer.replace("fn collect", "pub(super) fn collect"),
    );
    for mixed in [false, true] {
        if mixed {
            repo.write("README.md", "Rust dispatch\n");
            repo.write("src/lib.rs", format!("{library}// module list\n"));
        }
        repo.stage_all();
        for args in [&["--git"][..], &["--git", "--staged"][..]] {
            let result = repo.run_json("context", args);
            let changes = result["changes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|change| change["path"] == "src/producer.rs")
                .unwrap();
            let lines = changes["impacted_callers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| {
                    row["path"] == "src/consumer.rs"
                        && row["symbols"] == serde_json::json!(["collect"])
                })
                .map(|row| row["line"].as_u64().unwrap())
                .collect::<BTreeSet<_>>();
            assert_eq!(
                lines,
                BTreeSet::from([5, 6, 7, 8, 10]),
                "mixed={mixed} {args:?}: {changes}"
            );
        }
    }
}

#[test]
fn rust_explicit_trait_call_retains_a_changed_trait_signature() {
    for origin in ["producer", "dispatch"] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname='dispatch'\nversion='0.1.0'\nedition='2024'\n",
        );
        repo.write("src/lib.rs", "mod producer; mod consumer; mod dispatch;\n");
        let inherent = "pub(super) struct Names;\nimpl Names {\n    fn collect() -> u8 { 2 }\n}\n";
        let dispatch = "pub(super) trait Gather { fn collect() -> u8; }\nimpl Gather for crate::producer::Names { fn collect() -> u8 { 1 } }\n";
        let write = |inherent: &str, dispatch: &str| {
            if origin == "dispatch" {
                repo.write("src/producer.rs", inherent);
                repo.write("src/dispatch.rs", dispatch);
            } else {
                repo.write("src/producer.rs", format!("{inherent}{dispatch}"));
                repo.write("src/dispatch.rs", "");
            }
        };
        write(inherent, dispatch);
        repo.write(
        "src/consumer.rs",
        format!("use crate::producer::Names; use crate::{origin}::Gather;\nfn run() -> u8 {{ <Names as Gather>::collect() }}\n"),
    );
        repo.init_git();
        repo.commit_all("base");
        write(
            &inherent.replace("fn collect", "pub(super) fn collect"),
            &dispatch.replace("collect()", "collect(_: u8)"),
        );
        repo.stage_all();
        for args in [&["--git"][..], &["--git", "--staged"][..]] {
            let result = repo.run_json("context", args);
            let path = format!("src/{origin}.rs");
            let change = result["changes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["path"] == path)
                .unwrap();
            let rows = change["impacted_callers"].as_array().unwrap();
            assert!(
                rows.iter().any(|row| row["path"] == "src/consumer.rs"
                    && row["line"] == 1
                    && row["symbols"] == serde_json::json!(["Gather", "collect"])),
                "{args:?} {result}"
            );
        }
    }
}

#[test]
fn rust_private_method_keeps_calls_to_the_changed_receiver_among_same_named_methods() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='receivers'\nversion='0.1.0'\nedition='2021'\n",
    );
    let lib = "struct Item;\nimpl Item {\n    fn get(&self) -> i32 { 1 }\n}\nstruct Other;\nimpl Other {\n    fn get(&self) -> i32 { 2 }\n}\nfn near(item: &Item) -> i32 {\n    item.get()\n}\nfn independent(other: &Other) -> i32 {\n    other.get()\n}\nmod child;\n";
    repo.write("src/lib.rs", lib);
    repo.write("src/child.rs", "use super::Item as Actual;\nfn far(item: &Actual) -> i32 {\n    item.get()\n}\nfn independent(other: &super::Other) -> i32 {\n    other.get()\n}\nfn mixed(item: &Actual, other: &super::Other) -> i32 {\n    other.get() + super::Item::get(item)\n}\n");
    repo.init_git();
    repo.commit_all("base");
    for (owner, expected) in [
        (
            "Item",
            BTreeSet::from([("src/lib.rs", 9), ("src/child.rs", 2), ("src/child.rs", 8)]),
        ),
        (
            "Other",
            BTreeSet::from([("src/lib.rs", 12), ("src/child.rs", 5), ("src/child.rs", 8)]),
        ),
        (
            "both",
            BTreeSet::from([
                ("src/lib.rs", 9),
                ("src/lib.rs", 12),
                ("src/child.rs", 2),
                ("src/child.rs", 5),
                ("src/child.rs", 8),
            ]),
        ),
    ] {
        let mut changed = if owner == "both" {
            lib.replace("fn get(&self)", "fn get(&mut self)")
        } else {
            lib.replace(
                &format!("impl {owner} {{\n    fn get(&self)"),
                &format!("impl {owner} {{\n    fn get(&mut self)"),
            )
        };
        if owner == "Other" {
            changed.push_str("const TAG: u8 = 1;\n");
        }
        repo.write("src/lib.rs", changed);
        for staged in [false, true] {
            let flags = if staged {
                repo.stage_all();
                &["--git", "--staged"][..]
            } else {
                &["--git"][..]
            };
            let context = repo.run_json("context", flags);
            let actual: BTreeSet<_> = context["changes"][0]["impacted_callers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["symbols"].as_array().unwrap().iter().any(|s| s == "get"))
                .map(|c| (c["path"].as_str().unwrap(), c["line"].as_u64().unwrap()))
                .collect();
            assert_eq!(
                actual, expected,
                "owner={owner}, staged={staged}: {context}"
            );
        }
    }
}

#[test]
fn rust_private_method_keeps_deref_calls_outside_a_default_specialization() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='specialized'\nversion='0.1.0'\nedition='2021'\n",
    );
    let lib = r#"struct Item;
impl Item {
    fn get(&self) -> u8 { 1 }
}
struct Other<T=u8>(T, Item);
impl Other {
    fn get(&self) -> u8 { 2 }
}
impl<T> std::ops::Deref for Other<T> {
    type Target = Item;
    fn deref(&self) -> &Item { &self.1 }
}
fn read(other: &Other<u16>) -> u8 {
    other.get()
}
trait Read { fn get(&self) -> u8; }
mod child;
"#;
    repo.write("src/lib.rs", lib);
    repo.write("src/child.rs", "use super::{Item, Other};\nfn selected(#[cfg(all())] v: &Item, #[cfg(any())] v: &Other) -> u8 {\n    v.get()\n}\n");
    repo.init_git();
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        lib.replacen("fn get(&self)", "fn get(&mut self)", 1),
    );
    for staged in [false, true] {
        let flags = if staged {
            repo.stage_all();
            &["--git", "--staged"][..]
        } else {
            &["--git"][..]
        };
        let context = repo.run_json("context", flags);
        let calls = context["changes"][0]["impacted_callers"]
            .as_array()
            .unwrap();
        assert!(
            calls.iter().any(|c| c["name"] == "read"
                && c["line"] == 13
                && c["symbols"] == serde_json::json!(["get"])),
            "{context}"
        );
        let has_child = |change: &serde_json::Value, bucket: &str| {
            change[bucket].as_array().into_iter().flatten().any(|c| {
                c["path"] == "src/child.rs"
                    && c["line"] == 2
                    && c["symbols"] == serde_json::json!(["get"])
            })
        };
        if staged {
            // 生の index では条件付き引数が指す型を確定できない。
            assert!(
                has_child(&context["changes"][0], "low_confidence_callers"),
                "{context}"
            );
            let review = repo.run_json("review", flags);
            assert!(
                has_child(&review["impact"]["changes"][0], "impacted_callers"),
                "{review}"
            );
        } else {
            assert!(
                has_child(&context["changes"][0], "impacted_callers"),
                "{context}"
            );
        }
    }
}

#[test]
fn rust_same_file_method_calls_respect_receiver_types() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='receivers'\nversion='0.1.0'\nedition='2021'\n",
    );
    let old = r#"use std::collections::HashMap;
struct Cache;
struct Data { bytes: Vec<u8> }
struct Wrapper(Cache);
impl std::ops::Deref for Wrapper {
    type Target = Cache;
    fn deref(&self) -> &Self::Target { &self.0 }
}
impl Cache {
    fn get(&self, _: usize) -> usize { 0 }
}
fn callers(cache: &Cache, data: &Data, bytes: &[u8], map: &HashMap<usize, u8>) {
    let _ = cache.get(0);
    let _ = bytes.get(0);
    let _ = data.bytes.get(0);
    let _ = map.get(&0);
    let _ = (bytes.get(0), cache.get(0));
    { let bytes = cache; let _ = bytes.get(0); }
}
fn indirect(wrapper: &Wrapper) { let _ = wrapper.get(0); }
fn aliased(cache: &Cache) { type Vec<T> = (Cache, T); let bytes: Vec<u8> = (Cache, 0); let _ = bytes.0.get(0); }
"#;
    repo.write("src/lib.rs", old);
    repo.init_git();
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        old.replace("fn get(&self", "fn get(&mut self"),
    );
    for staged in [false, true] {
        let flags = if staged {
            repo.stage_all();
            repo.write("src/lib.rs", old);
            &["--git", "--staged"][..]
        } else {
            &["--git"][..]
        };
        let context = repo.run_json("context", flags);
        let actual: BTreeSet<_> = context["changes"][0]["impacted_callers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|caller| {
                caller["symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s == "get")
            })
            .map(|caller| {
                old.lines()
                    .nth(caller["line"].as_u64().unwrap() as usize)
                    .unwrap()
                    .trim()
            })
            .collect();
        let expected: BTreeSet<_> = old
            .lines()
            .map(str::trim)
            .filter(|line| {
                line.contains("cache.get(0)")
                    || line.contains("let bytes = cache;")
                    || line.contains("wrapper.get(0)")
                    || line.contains("bytes.0.get(0)")
            })
            .collect();
        assert_eq!(actual, expected, "staged={staged}");
    }
}
#[test]
fn rust_same_file_method_results_check_inherent_dispatch_and_factory_owner() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='methods'\nversion='0.1.0'\nedition='2021'\n",
    );
    repo.write(
        "src/lib.rs",
        "mod reader; mod catalog; mod extension;\nfn flag(include: bool) { assert!(include); }\n",
    );
    let provider = "pub struct Records;\nimpl Records {\n    pub fn open() -> Self { Records }\n    pub fn get(&self, _: &str) -> bool { true }\n}\n";
    repo.write("src/catalog.rs", provider);
    let plain = r#"use crate::catalog as cards;
pub(crate) struct Store;
pub(crate) struct Data { bytes: Vec<u8> }
pub(crate) struct Other { bytes: Store }
impl Store {
    fn get(&mut self, _: usize) -> Option<&Data> { None }
}
fn view(store: &mut Store) {
    let Some(data) = store.get(0) else { return };
    let _ = data.bytes.get(0);
    let records = cards::Records::open();
    let _ = records.get("name");
}
fn check_name(include: bool) { assert!(include); let _text = include_str!("catalog.rs"); }
"#;
    repo.write("src/reader.rs", plain);
    repo.write("src/extension.rs", "use crate::reader::{Store, Other};\npub trait Read {fn get(&mut self, key: usize) -> Option<&Other>;}\nimpl Read for Store {fn get(&mut self, _:usize) -> Option<&Other>{None}}\n");
    repo.init_git();
    repo.commit_all("base");
    let changed = plain.replace("fn get(&mut self", "fn get(&self");
    let rows = |cx: &serde_json::Value| -> BTreeSet<u64> {
        cx["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["path"] == "src/reader.rs")
            .flat_map(|c| c["impacted_callers"].as_array().into_iter().flatten())
            .filter(|c| c["symbols"].as_array().unwrap().iter().any(|n| n == "get"))
            .map(|c| c["line"].as_u64().unwrap())
            .collect()
    };
    let lines = |repo: &TestRepo, args: &[&str]| rows(&repo.run_json("context", args));
    repo.write("src/reader.rs", &changed);
    assert_eq!(lines(&repo, &["--git"]), BTreeSet::from([8]));
    repo.stage_all();
    repo.write(
        "src/catalog.rs",
        provider.replace(
            "-> Self { Records }",
            "-> crate::reader::Store { crate::reader::Store }",
        ),
    );
    assert_eq!(lines(&repo, &["--git", "--staged"]), BTreeSet::from([8]));
    assert_eq!(
        rows(&repo.run_json("review", &["--git", "--staged"])["impact"]),
        BTreeSet::from([8])
    );
    assert!(lines(&repo, &["--git"]).contains(&11));
    repo.write("src/catalog.rs", provider);
    let bound = plain.replace(
        "use crate::catalog as cards;",
        "use crate::catalog as cards; use crate::extension::Read;",
    );
    repo.write("src/reader.rs", &bound);
    repo.commit_all("bring the competing trait into scope");
    repo.write(
        "src/reader.rs",
        bound.replace("fn get(&mut self", "fn get(&self"),
    );
    assert_eq!(lines(&repo, &["--git"]), BTreeSet::from([8, 9]));
    for input in [
        "#[doc = include_str!(\"catalog.rs\")] struct Guide;",
        "fn read_docs() { let _text = include_str!(concat!(\"catalog\", \".rs\")); }",
        "macro_rules! include_str { ($p:literal) => { () }; } fn read_docs() { let _text = include_str!(\"catalog.rs\"); }",
    ] {
        repo.write("src/reader.rs", format!("{plain}{input}\n"));
        repo.commit_all("unverified Rust text source");
        repo.write("src/reader.rs", &changed);
        assert!(lines(&repo, &["--git"]).contains(&9), "{input}");
    }
}

/// 別の型の `new` を呼ぶ行 (crate の型、`use` した std の型、prelude の型とモジュールの自由関数、
/// std のフルパス)。
const OTHER_CALLS: [&str; 4] = [
    "let engine = Engine::new();",
    "let shared = Arc::new(Mutex::new(0));",
    "let boxed = Box::new(config::new());",
    "let worker = std::thread::Builder::new().spawn(move || *shared.lock().unwrap());",
];

/// ipc・engine・tip の 3 つのクレートのワークスペース。engine は ipc に依存する。
fn workspace(server: &str, admin: &str) -> TestRepo {
    let repo = TestRepo::new();
    for package in ["ipc", "engine", "tip"] {
        repo.create_dir_all(format!("{package}/src"));
        let dependencies = if package == "ipc" {
            ""
        } else {
            "\n[dependencies]\nipc = { path = \"../ipc\" }\n"
        };
        repo.write(
            format!("{package}/Cargo.toml"),
            format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{dependencies}"),
        );
    }
    repo.create_dir_all("engine/src/server");
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"ipc\", \"engine\", \"tip\"]\nresolver = \"2\"\n",
    );
    repo.write("ipc/src/lib.rs", "pub fn ping() -> bool {\n    true\n}\n");
    repo.write(
        "tip/src/lib.rs",
        "pub fn show() -> bool {\n    ipc::ping()\n}\n",
    );
    repo.write("engine/src/lib.rs", LIB);
    repo.write("engine/src/config.rs", CONFIG);
    repo.write("engine/src/server.rs", server);
    repo.write("engine/src/server/admin.rs", admin);
    repo.init_git();
    repo.commit_all("initial");
    repo
}

/// `engine/src/server/admin.rs` の変更の、`symbol` の呼び出し元の行の本文 (前後の空白を除く)。
/// (blocking の一覧, low・informational を含む全部の一覧) を返す。
fn caller_lines(
    repo: &TestRepo,
    symbol: &str,
    args: &[&str],
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut arguments = vec!["--git"];
    arguments.extend_from_slice(args);
    let context = repo.run_json("context", &arguments);
    let change = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "engine/src/server/admin.rs")
        .unwrap_or_else(|| panic!("admin.rs is not analyzed: {context}"));
    let lines = |key: &str| -> BTreeSet<String> {
        change[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|caller| {
                caller["path"] != "engine/src/server/admin.rs"
                    && caller["symbols"]
                        .as_array()
                        .is_some_and(|symbols| symbols.iter().any(|name| name == symbol))
            })
            .map(|caller| repo.caller_source_line(caller))
            .collect()
    };
    let blocking = lines("impacted_callers");
    let mut all = blocking.clone();
    all.extend(lines("low_confidence_callers"));
    all.extend(lines("informational_callers"));
    (blocking, all)
}

/// `server.rs` の末尾に `extra` を足し、`Administration::new` の引数を変えたときの `new` の呼び出し元。
fn new_callers(extra: &str, args: &[&str]) -> (BTreeSet<String>, BTreeSet<String>) {
    let repo = workspace(&format!("{SERVER}{extra}"), ADMIN_OLD);
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    if args.contains(&"--staged") {
        repo.stage_all();
    }
    caller_lines(&repo, "new", args)
}

#[test]
fn impact_rust_owner_paths_drop_calls_on_other_types() {
    // 所有型と違う名前の型・モジュールのパスの `new` は、low・informational にも出さない。
    let (blocking, all) = new_callers("", &[]);
    assert_eq!(blocking, BTreeSet::from([ADMIN_CALL.to_owned()]));
    assert_eq!(all, blocking);
}

#[test]
fn impact_rust_owner_paths_keep_unresolved_bindings() {
    for (extra, expected) in [
        (
            "use admin::Administration as Other;\nfn retry() {\n    let _ = Other::new(None);\n}\n",
            "let _ = Other::new(None);",
        ),
        (
            "type Other = admin::Administration;\nfn retry() {\n    let _ = Other::new(None);\n}\n",
            "let _ = Other::new(None);",
        ),
        (
            "impl admin::Administration {\n    fn retry() -> Self {\n        Self::new(None)\n    }\n}\n",
            "Self::new(None)",
        ),
        (
            "fn retry() {\n    use admin::Administration as Engine;\n    let _ = Engine::new(None);\n}\n",
            "let _ = Engine::new(None);",
        ),
        (
            "fn retry() {\n    type Box = admin::Administration;\n    let _ = Box::new(None);\n}\n",
            "let _ = Box::new(None);",
        ),
    ] {
        let (blocking, _) = new_callers(extra, &[]);
        assert!(blocking.contains(expected), "{blocking:?}");
        assert!(blocking.contains(ADMIN_CALL), "{blocking:?}");
    }
    let (blocking, _) = new_callers("", &["--staged"]);
    assert!(blocking.contains(OTHER_CALLS[0]), "{blocking:?}");
}

#[test]
fn impact_rust_owner_paths_keep_trait_and_aliased_impl_targets() {
    for admin in [
        "pub(super) struct Administration;\npub(super) trait Build { fn new(x: u8) -> Self; }\nimpl Build for Administration {\n    fn new(x: u8) -> Self { Self }\n}\n",
        "use super::*;\ntype Administration = Engine;\nimpl Administration {\n    pub(super) fn new(x: u8) -> Self { Engine }\n}\n",
    ] {
        let repo = workspace(SERVER, admin);
        repo.write(
            "engine/src/server/admin.rs",
            admin.replace("x: u8", "x: u16"),
        );
        let (blocking, _) = caller_lines(&repo, "new", &[]);
        assert!(blocking.contains(OTHER_CALLS[0]), "{blocking:?}");
    }
}

const RECEIVERS: &str = r#"pub fn dispatch(engine: &mut Engine, manage: u32, tx: u32) -> std::io::Result<()> {
    let admin = admin::Administration::new(config::Config::default());
    engine.handle(manage);
    admin.handle(manage, tx)?;
    Ok(())
}
"#;

#[test]
fn impact_rust_owner_paths_keep_only_the_changed_constructor_and_receiver() {
    let old = ADMIN_OLD.replace("fn handle(&self)", "fn handle(&self, manage: u32, tx: u32)");
    let new = ADMIN_NEW.replace("fn handle(&self)", "fn handle(&self, manage: u32, tx: u64)");
    let server =
        format!("use crate::{{Engine, config}};\nmod admin;\nuse std::sync::Mutex;\n{RECEIVERS}");
    let repo = workspace(&server, &old);
    repo.write("engine/src/server/admin.rs", new);
    let output = cargo_bin()
        .current_dir(repo.root())
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected: Vec<_> = server
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            line.contains("admin::Administration::new(")
                || line.contains("admin.handle(manage, tx)?")
        })
        .map(|(index, line)| {
            format!(
                "→ engine/src/server.rs:{} [{}]",
                index + 1,
                if line.contains("::new(") {
                    "new"
                } else {
                    "handle"
                }
            )
        })
        .collect();
    let actual: Vec<_> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('→'))
        .collect();
    assert_eq!(actual, expected, "{stderr}");
}

#[test]
fn impact_rust_owner_paths_keep_shadowed_receivers_and_deref_targets() {
    let extra = r#"
fn shadow(engine: &mut Engine, admin: &admin::Administration) {
    if true {
        let engine = admin;
        engine.handle();
    }
}
fn forwarded(wrapper: &Wrapper) {
    wrapper.handle();
}
"#;
    let old = ADMIN_OLD.replace(
        "pub(super) struct Administration",
        "pub struct Administration",
    );
    let new = ADMIN_NEW.replace(
        "pub(super) struct Administration",
        "pub struct Administration",
    );
    let repo = workspace(
        &format!("use crate::Wrapper;\n{SERVER}pub use admin::Administration as Admin;\n{extra}"),
        &old,
    );
    repo.write("engine/src/lib.rs", format!("{LIB}pub struct Wrapper(server::Admin);\nimpl std::ops::Deref for Wrapper {{\n    type Target = server::Admin;\n    fn deref(&self) -> &Self::Target {{ &self.0 }}\n}}\n"));
    repo.commit_all("wrapper");
    repo.write(
        "engine/src/server/admin.rs",
        new.replace("fn handle(&self)", "fn handle(&self, detail: u8)"),
    );
    let (blocking, _) = caller_lines(&repo, "handle", &[]);
    for expected in ["engine.handle();", "wrapper.handle();", "admin.handle()"] {
        assert!(blocking.contains(expected), "{blocking:?}");
    }
}

#[test]
fn impact_rust_owner_paths_require_generic_bounds_before_excluding_deref() {
    let library = r#"pub mod config;
pub mod server;
use ipc::Backend;
pub struct Engine<B>(pub B, pub server::Admin);
impl<B: Backend> Engine<B> {
    pub fn handle(&self) {}
}
impl<B> std::ops::Deref for Engine<B> {
    type Target = server::Admin;
    fn deref(&self) -> &Self::Target { &self.1 }
}
"#;
    let server = r#"use crate::{Engine, config};
use ipc::{Active, Inactive};
use std::sync::Mutex;
mod admin;
pub use admin::Administration as Admin;
fn first(active: &Engine<Active>) {
    active.handle();
}
fn second(inactive: &Engine<Inactive>) {
    inactive.handle();
}
"#;
    let old = ADMIN_OLD.replace(
        "pub(super) struct Administration",
        "pub struct Administration",
    );
    let repo = workspace(server, &old);
    repo.write("ipc/src/lib.rs", "pub trait Backend {}\npub struct Active;\npub struct Inactive;\nimpl Backend for Active {}\npub fn ping() -> bool { true }\n");
    repo.write("engine/src/lib.rs", library);
    repo.commit_all("generic receivers");
    repo.write(
        "engine/src/server/admin.rs",
        old.replace("fn handle(&self)", "fn handle(&self, detail: u8)"),
    );
    let (blocking, all) = caller_lines(&repo, "handle", &[]);
    assert!(blocking.contains("inactive.handle();"), "{blocking:?}");
    assert!(!all.contains("active.handle();"), "{all:?}");
}

#[test]
fn impact_rust_owner_paths_keep_calls_inside_an_attributed_method() {
    let server = format!(
        "{SERVER}\npub struct Server;\nimpl Server {{\n    #[injector::with_prelude]\n    fn retry() {{\n        let local = Engine::new();\n        let shared_retry = Arc::new(Mutex::new(0));\n    }}\n}}\n"
    );
    let repo = workspace(&server, ADMIN_OLD);
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    let (blocking, all) = caller_lines(&repo, "new", &[]);
    for line in [
        ADMIN_CALL,
        "let local = Engine::new();",
        "let shared_retry = Arc::new(Mutex::new(0));",
    ] {
        assert!(blocking.contains(line), "{blocking:?}");
    }
    assert!(!all.contains(OTHER_CALLS[0]), "{all:?}");
}

#[test]
fn impact_rust_owner_paths_allow_an_unrelated_body_change() {
    let repo = workspace(SERVER, ADMIN_OLD);
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    repo.write(
        "tip/src/lib.rs",
        "pub fn show() -> bool { ipc::ping() && true }\n",
    );
    assert_eq!(
        caller_lines(&repo, "new", &[]).1,
        BTreeSet::from([ADMIN_CALL.to_owned()])
    );
    // 旧側で別のファイルをこのモジュールに取り込んでいた場合は配置が変わる。
    repo.write("engine/src/server/admin.rs", ADMIN_OLD);
    repo.write(
        "tip/src/lib.rs",
        "include!(\"../../engine/src/server.rs\");\n",
    );
    repo.commit_all("alternate placement");
    repo.write("engine/src/server/admin.rs", ADMIN_NEW);
    repo.write("tip/src/lib.rs", "pub fn show() -> bool { ipc::ping() }\n");
    assert!(caller_lines(&repo, "new", &[]).0.contains(OTHER_CALLS[0]));
}

/// `#[derive(Debug)]` が作る `Debug::fmt` も `fmt` の宣言 (inherent メソッドでない宣言) なので、
/// 変更した inherent の `fmt` の所有型で、ほかのパスの `fmt` の呼び出しを除かない。
#[test]
fn impact_rust_owner_paths_keep_trait_calls_of_a_derived_method_name() {
    let old = "use super::*;\n#[derive(Debug)]\npub(super) struct Administration;\nimpl Administration {\n    pub(super) fn fmt(&self) -> String {\n        String::new()\n    }\n}\n";
    let new = old.replace(
        "fn fmt(&self) -> String {\n        String::new()",
        "fn fmt(&self, verbose: bool) -> String {\n        verbose.to_string()",
    );
    // `Debug` の名前を import すると derive の差し替えを確かめられないので、module を別名で使う。
    let server = "use crate::config;\nmod admin;\nuse std::fmt as formatting;\nuse std::sync::Mutex;\npub fn show(admin: &admin::Administration, f: &mut formatting::Formatter<'_>) -> formatting::Result {\n    formatting::Debug::fmt(admin, f)\n}\n";
    let repo = workspace(server, old);
    repo.write("engine/src/server/admin.rs", new);
    let (_, all) = caller_lines(&repo, "fmt", &[]);
    assert!(all.contains("formatting::Debug::fmt(admin, f)"), "{all:?}");
}

#[test]
fn impact_rust_owner_paths_allow_unshadowed_builtin_derive() {
    let derive =
        |admin: &str| admin.replace("pub(super) struct", "#[derive(Debug)]\npub(super) struct");
    let repo = workspace(SERVER, &derive(ADMIN_OLD));
    repo.write("engine/src/server/admin.rs", derive(ADMIN_NEW));
    assert_eq!(
        caller_lines(&repo, "new", &[]).1,
        BTreeSet::from([ADMIN_CALL.to_owned()])
    );
    let repo = workspace(
        &format!("use injector::Custom as Debug;\n{SERVER}"),
        &derive(ADMIN_OLD),
    );
    repo.write("engine/src/server/admin.rs", derive(ADMIN_NEW));
    assert!(caller_lines(&repo, "new", &[]).0.contains(OTHER_CALLS[0]));
}

#[test]
fn rust_function_pointers_distinguish_qualified_module_declarations() {
    let repo = TestRepo::new();
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers=['a','b']\nresolver='3'\n",
    );
    for (name, dependencies) in [("a", ""), ("b", "[dependencies]\na={path='../a'}")] {
        repo.create_dir_all(format!("{name}/src"));
        repo.write(
            format!("{name}/Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n{dependencies}\n"),
        );
    }
    let foreign = |module, link, ty| {
        format!(
            "pub mod {module} {{\n unsafe extern \"C\" {{\n #[link_name=\"{link}\"]\n pub fn translate(x:{ty});\n }}\n}}\n"
        )
    };
    let producer = |ty| {
        format!(
            "{}{}pub mod forwarded {{ pub use crate::api::translate; }}\nfn poll(_:u8) {{}}\npub mod ptr {{\n pub const OWN:fn(u8)=super::poll;\n}}\n",
            foreign("api", "main_translate", ty),
            foreign("other", "other_translate", "u8")
        )
    };
    repo.write("a/src/lib.rs", producer("u8"));
    let readers = "pub const ONE:unsafe extern \"C\" fn(u8)=a::other::translate;\npub const TWO:unsafe extern \"C\" fn(u8)=a::api::translate;\npub const THREE:unsafe extern \"C\" fn(u8)=a::forwarded::translate;\n";
    repo.write(
        "b/src/lib.rs",
        format!("{readers}mod check;\npub fn poll(_:u8) {{}}\n"),
    );
    repo.write("b/src/check.rs", "pub const CHECK:fn(u8)=crate::poll;\n");
    repo.init_git();
    repo.commit_all("initial");
    let lines = |context: &serde_json::Value, target: &str, consumer: &str| {
        context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ch| ch["path"] == target)
            .unwrap()["impacted_callers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| row["path"] == consumer)
            .map(|row| repo.caller_source_line(row))
            .collect::<BTreeSet<_>>()
    };
    repo.write("a/src/lib.rs", producer("i32"));
    repo.stage_all();
    for extra in [&["--git"][..], &["--git", "--staged"]] {
        let rows = lines(
            &repo.run_json("context", extra),
            "a/src/lib.rs",
            "b/src/lib.rs",
        );
        assert_eq!(rows, readers.lines().skip(1).map(str::to_owned).collect());
    }
    repo.write("a/src/lib.rs", producer("u8"));
    let staged = repo.run_json("review", &["--git", "--staged"]);
    assert_eq!(
        lines(&staged["impact"], "a/src/lib.rs", "b/src/lib.rs"),
        readers.lines().skip(1).map(str::to_owned).collect()
    );
    repo.stage_all();
    repo.write(
        "b/src/lib.rs",
        format!("{readers}mod check;\npub fn poll(_:i32) {{}}\n"),
    );
    repo.stage_all();
    for extra in [&["--git"][..], &["--git", "--staged"]] {
        let cx = repo.run_json("context", extra);
        assert!(
            lines(&cx, "b/src/lib.rs", "a/src/lib.rs").is_empty(),
            "{cx}"
        );
        assert!(
            !lines(&cx, "b/src/lib.rs", "b/src/check.rs").is_empty(),
            "{cx}"
        );
    }
    let via_root = "extern crate self as a;\nextern crate a as dependency;\npub mod other {pub use crate::dependency::api::translate;}\npub const ALIAS:unsafe extern \"C\" fn(u8)=::a::other::translate;\n";
    repo.write("b/src/lib.rs", via_root);
    repo.commit_all("extern crate alias");
    repo.write("a/src/lib.rs", producer("i32"));
    repo.stage_all();
    let pointer = via_root.lines().last().unwrap();
    for extra in [&["--git"][..], &["--git", "--staged"]] {
        let cx = repo.run_json("context", extra);
        assert!(
            lines(&cx, "a/src/lib.rs", "b/src/lib.rs").contains(pointer),
            "{cx}"
        );
    }
}

#[test]
fn same_file_rust_free_function_calls_keep_only_possible_owners() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname='owners'\nversion='0.1.0'\nedition='2024'\n",
    );
    repo.write("src/other.rs", "pub(super) fn collect<T: Default>() -> T { T::default() }\npub(super) struct Batch;\nimpl Batch { pub(super) fn collect<T: Default>() -> T { T::default() } }\n");
    repo.write("src/forward.rs", "pub(super) use crate::collect;\n");
    let old = r#"mod other;
mod forward;
fn collect(n: u8) -> u8 { n }
fn caller() {
    let _ = collect(3);
    let _ = self::collect(3);
    let _ = crate::collect(3);
    let _ = forward::collect(3);
    let _: (u8, _) = (other::collect(), collect(3));
}
fn unrelated() {
    let _: Vec<u8> = (0u8..3).collect();
    let _: u8 = other::collect();
    let _: u8 = other::Batch::collect();
    let _ = (0u8..3).collect::<Vec<_>>();
    let _ = other::collect::<u8>();
    let _ = other::Batch::collect::<u8>();
}
fn shadowed() {
    use crate as other;
    let _ = other::collect(3);
}
"#;
    repo.write("src/lib.rs", old);
    repo.init_git();
    repo.commit_all("base");
    let new = old.replace("collect(n: u8)", "collect(n: u8, _: bool)");
    for updated in [false, true] {
        repo.write(
            "src/lib.rs",
            if updated {
                new.replace("collect(3)", "collect(3, true)")
            } else {
                new.clone()
            },
        );
        for staged in [false, true] {
            let args = if staged {
                repo.stage_all();
                &["--git", "--staged"][..]
            } else {
                &["--git"][..]
            };
            let context = repo.run_json("context", args);
            let actual: BTreeSet<_> = context["changes"][0]["impacted_callers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| row["path"] == "src/lib.rs")
                .map(|row| row["line"].as_u64().unwrap())
                .collect();
            let wanted = if updated {
                BTreeSet::new()
            } else {
                (4..=8).chain([20]).collect()
            };
            assert_eq!(
                actual, wanted,
                "updated={updated} staged={staged} {context}"
            );
        }
    }
}
