//! Rust の関数に `const` を足すだけの変更で Stop フックが止まらないか (Issue #113)。

use super::support::*;

const REASON: &str = "rust_const_fn_added";

const SIDE_ENUM: &str = "#[derive(Clone, Copy)]\npub enum Side {\n    Left,\n    Right,\n}\n\n";
const SIDE_BODY: &str = " {\n    match side {\n        Side::Left => \"left\",\n        Side::Right => \"right\",\n    }\n}\n";
const SIDE_CALLER: &str = "use crate::side::{Side, side_label};\n\npub fn show() -> &'static str {\n    side_label(Side::Left)\n}\n";
const COUNTER_OLD: &str = "pub struct Counter {\n    items: [u8; 4],\n}\n\nimpl Counter {\n    pub fn len(&self) -> usize {\n        self.items.len()\n    }\n}\n";
const COUNTER_CALLER: &str = "use crate::side::Counter;\n\npub fn count(counter: &Counter) -> usize {\n    counter.len()\n}\n";

fn side(header: &str) -> String {
    format!("{SIDE_ENUM}{header}{SIDE_BODY}")
}

/// `src/side.rs` を `old` で commit したリポジトリ。
fn base_repo(old: &str, caller: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"labels\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    repo.write("src/lib.rs", "pub mod caller;\npub mod side;\n");
    repo.write("src/side.rs", old);
    repo.write("src/caller.rs", caller);
    repo.init_git();
    repo.commit_all("base");
    repo
}

/// `src/side.rs` を `old` で commit し、`new` に書き換えた (commit しない) リポジトリ。
fn repo_with_change(old: &str, new: &str, caller: &str) -> TestRepo {
    let repo = base_repo(old, caller);
    repo.write("src/side.rs", new);
    repo
}

/// `command --git --hook [extra]` の終了コードと stderr。
fn run_hook(repo: &TestRepo, command: &str, extra: &[&str]) -> (Option<i32>, String) {
    let output = cargo_bin()
        .args([command, "--git", "--hook"])
        .args(extra)
        .arg("--dir")
        .arg(repo.root())
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn api_entries<'a>(hook: &'a serde_json::Value, key: &str) -> Vec<&'a serde_json::Value> {
    hook["api"][key]
        .as_array()
        .map(|entries| entries.iter().collect())
        .unwrap_or_default()
}

fn has_mod_compat(hook: &serde_json::Value, name: &str) -> bool {
    api_entries(hook, "mod_compat")
        .iter()
        .any(|entry| entry["n"] == name && entry["reason"] == REASON)
}

/// 呼び出し元 `src/caller.rs:3` が `group_key` (`impacts` / `impact_info`) にあるか。
fn lists_caller(hook: &serde_json::Value, group_key: &str, symbol: &str) -> bool {
    hook[group_key].as_array().is_some_and(|groups| {
        groups.iter().any(|group| {
            group["refs"].as_array().is_some_and(|refs| {
                refs.iter().any(|reference| {
                    reference["p"] == "src/caller.rs"
                        && reference["ln"] == 3
                        && reference["s"]
                            .as_array()
                            .is_some_and(|symbols| symbols.iter().any(|s| s == symbol))
                })
            })
        })
    })
}

/// const を足しただけなら review は `mod_compat`、impacts なし、終了コード 0。impact も止めない。
fn assert_compatible_hooks(repo: &TestRepo, extra: &[&str], name: &str, caller_symbol: &str) {
    let (code, stderr) = run_hook(repo, "review", extra);
    assert_eq!(code, Some(0), "review {extra:?}: {stderr}");
    let hook: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert!(has_mod_compat(&hook, name), "{hook}");
    assert!(api_entries(&hook, "mod").is_empty(), "{hook}");
    assert!(hook.get("impacts").is_none(), "{hook}");
    // 呼び出し元は消さず、確認用の情報提供に残す。
    assert!(lists_caller(&hook, "impact_info", caller_symbol), "{hook}");
    let (code, stderr) = run_hook(repo, "impact", extra);
    assert_eq!(code, Some(0), "impact {extra:?}: {stderr}");
}

fn assert_compatible(old: &str, new: &str, caller: &str, name: &str, caller_symbol: &str) {
    let repo = repo_with_change(old, new, caller);
    assert_compatible_hooks(&repo, &[], name, caller_symbol);
}

/// 従来どおり止まる変更は、review が `api_key` (`mod` / `rm`) と impacts を出し、impact も止める。
fn assert_blocking_hooks(repo: &TestRepo, extra: &[&str], name: &str, api_key: &str) {
    let (code, stderr) = run_hook(repo, "review", extra);
    assert_eq!(code, Some(1), "review {extra:?}: {stderr}");
    let hook: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert!(
        api_entries(&hook, api_key)
            .iter()
            .any(|entry| entry["n"] == name),
        "{api_key}: {hook}"
    );
    assert!(api_entries(&hook, "mod_compat").is_empty(), "{hook}");
    assert!(
        hook["impacts"]
            .as_array()
            .is_some_and(|impacts| !impacts.is_empty()),
        "{hook}"
    );
    let (code, stderr) = run_hook(repo, "impact", extra);
    assert_eq!(code, Some(1), "impact {extra:?}: {stderr}");
}

fn assert_blocking(old: &str, new: &str, caller: &str, name: &str, api_key: &str) {
    let repo = repo_with_change(old, new, caller);
    assert_blocking_hooks(&repo, &[], name, api_key);
}

#[test]
fn adding_const_to_a_function_does_not_block_hooks() {
    assert_compatible(
        &side("pub fn side_label(side: Side) -> &'static str"),
        &side("pub const fn side_label(side: Side) -> &'static str"),
        SIDE_CALLER,
        "side_label",
        "side_label",
    );
}

#[test]
fn adding_const_to_an_inherent_method_does_not_block_hooks() {
    assert_compatible(
        COUNTER_OLD,
        &COUNTER_OLD.replace("pub fn len", "pub const fn len"),
        COUNTER_CALLER,
        "Counter.len",
        "len",
    );
}

/// `const` を関数名と別の行に書いても、修飾子の間にコメントがあっても同じ。
#[test]
fn const_on_its_own_line_or_beside_comments_does_not_block_hooks() {
    for (old, new) in [
        (
            "pub fn side_label(side: Side) -> &'static str",
            "pub const\nfn side_label(side: Side) -> &'static str",
        ),
        (
            "pub /* label */ fn side_label(side: Side) -> &'static str",
            "pub /* label */ const /* now */ fn side_label(side: Side) -> &'static str",
        ),
    ] {
        assert_compatible(
            &side(old),
            &side(new),
            SIDE_CALLER,
            "side_label",
            "side_label",
        );
    }
}

#[test]
fn removing_const_still_blocks_hooks() {
    assert_blocking(
        &side("pub const fn side_label(side: Side) -> &'static str"),
        &side("pub fn side_label(side: Side) -> &'static str"),
        SIDE_CALLER,
        "side_label",
        "mod",
    );
    assert_blocking(
        &COUNTER_OLD.replace("pub fn len", "pub const fn len"),
        COUNTER_OLD,
        COUNTER_CALLER,
        "Counter.len",
        "mod",
    );
}

#[test]
fn const_with_other_signature_changes_still_blocks_hooks() {
    let plain = "pub fn side_label(side: Side) -> &'static str";
    for new in [
        "pub const fn side_label(side: Side, loud: bool) -> &'static str",
        "pub const fn side_label(side: Side) -> &'static [u8]",
        "pub const fn side_label<T>(side: Side) -> &'static str",
        "pub const unsafe fn side_label(side: Side) -> &'static str",
        "pub const async fn side_label(side: Side) -> &'static str",
        "pub const extern \"C\" fn side_label(side: Side) -> &'static str",
    ] {
        assert_blocking(&side(plain), &side(new), SIDE_CALLER, "side_label", "mod");
    }
    assert_blocking(
        &side(plain),
        &side("pub(crate) const fn side_label(side: Side) -> &'static str"),
        SIDE_CALLER,
        "side_label",
        "rm",
    );
    assert_blocking(
        COUNTER_OLD,
        &COUNTER_OLD.replace("pub fn len(&self)", "pub const fn len(&mut self)"),
        COUNTER_CALLER
            .replace("counter: &Counter", "counter: &mut Counter")
            .as_str(),
        "Counter.len",
        "mod",
    );
}

/// ヘッダに現れる型の実体が同じファイルで変わると、`const` 以外の字面が同じでも呼び出しが
/// 壊れる (別名・`use` の別名の束縛先)。戻り値の `impl Trait` は本体の変更で auto trait が変わる。
#[test]
fn const_with_a_changed_type_meaning_still_blocks_hooks() {
    let take_caller = "use crate::side::take;\n\npub fn show() -> u8 {\n    take(1u8)\n}\n";
    for (old, new, caller, name) in [
        (
            "type Arg = u8;\n\npub fn take(_value: Arg) -> u8 {\n    0\n}\n",
            "type Arg = u16;\n\npub const fn take(_value: Arg) -> u8 {\n    0\n}\n",
            take_caller,
            "take",
        ),
        (
            "type Original = u8;\nuse self::Original as Arg;\n\npub fn take(_value: Arg) -> u8 {\n    0\n}\n",
            "type Original = u16;\nuse self::Original as Arg;\n\npub const fn take(_value: Arg) -> u8 {\n    0\n}\n",
            take_caller,
            "take",
        ),
        (
            "pub fn make() -> impl Copy {\n    0u8\n}\n",
            "pub const fn make() -> impl Copy {\n    core::marker::PhantomData::<*const ()>\n}\n",
            "fn need_send<T: Send>(_: T) {}\n\npub fn show() {\n    need_send(crate::side::make());\n}\n",
            "make",
        ),
        (
            "const VALUE: u8 = 0;\npub fn make() -> impl Copy { VALUE }\n",
            "const VALUE: *const () = core::ptr::null();\npub const fn make() -> impl Copy { VALUE }\n",
            "fn need_send<T: Send>(_: T) {}\n\npub fn show() {\n    need_send(crate::side::make());\n}\n",
            "make",
        ),
    ] {
        assert_blocking(old, new, caller, name, "mod");
    }
}

#[test]
fn const_proof_preserves_macro_spacing_and_nested_impl_conditions() {
    let caller = "use crate::side::take;\n\npub fn read() {\n    take([0; 5]);\n}\n";
    let old = "const N: usize = stringify!(a + b).len();\npub fn take(_: [u8; N]) {}\n";
    assert_compatible(
        old,
        &old.replace("pub fn take", "pub const fn take"),
        caller,
        "take",
        "take",
    );
    assert_blocking(
        old,
        "const N: usize = stringify!(a+b).len();\npub const fn take(_: [u8; N]) {}\n",
        caller,
        "take",
        "mod",
    );

    let source = |small: &str, large: &str, qualifier: &str| {
        format!(
            "pub struct S;\npub trait T {{ type A; }}\n#[cfg({small})]\nfn small() {{ impl T for S {{ type A = u8; }} }}\n#[cfg({large})]\nfn large() {{ impl T for S {{ type A = u16; }} }}\npub {qualifier}fn take(_: <S as T>::A) {{}}\n"
        )
    };
    let old = source("all()", "any()", "");
    let caller = "use crate::side::take;\n\npub fn read() {\n    take(1u8);\n}\n";
    assert_compatible(
        &old,
        &source("all()", "any()", "const "),
        caller,
        "take",
        "take",
    );
    assert_blocking(
        &old,
        &source("any()", "all()", "const "),
        caller,
        "take",
        "mod",
    );
}

#[test]
fn review_regressions_keep_generated_contract_changes_blocking() {
    let empty_impl = "#[derive(Clone, Copy)]\npub struct Token(*const ());\nunsafe impl Send for Token {}\npub fn make() -> impl Copy { Token(core::ptr::null()) }\n";
    let nested_macro = "pub struct S;\npub trait T { type A; }\nmacro_rules! install { ($ty:ty) => { impl T for S { type A = $ty; } }; }\nfn configure() { install!(u8); }\npub fn take(_: <S as T>::A) {}\n";
    let own_macro = nested_macro
        .replace("fn configure() { install!(u8); }\n", "")
        .replace(
            "pub fn take(_: <S as T>::A) {}",
            "pub fn take(_: <S as T>::A) { install!(u8); }",
        );
    let opaque_dependency = "const N: usize = 4;\nmacro_rules! define_arg { () => { type Arg = [u8; N]; }; }\ndefine_arg!();\npub fn take(_: Arg) {}\n";
    for (old, new, caller, name) in [
        (
            empty_impl,
            empty_impl.replace("unsafe impl Send for Token {}\n", ""),
            "fn need_send<T: Send>(_: T) {}\n\npub fn run() {\n    need_send(crate::side::make());\n}\n",
            "make",
        ),
        (
            nested_macro,
            nested_macro.replace("install!(u8)", "install!(u16)"),
            "use crate::side::take;\n\npub fn run() {\n    take(1u8);\n}\n",
            "take",
        ),
        (
            opaque_dependency,
            opaque_dependency.replace("N: usize = 4", "N: usize = 8"),
            "use crate::side::take;\n\npub fn run() {\n    take([0u8; 4]);\n}\n",
            "take",
        ),
        (
            own_macro.as_str(),
            own_macro.replace("install!(u8)", "install!(u16)"),
            "use crate::side::take;\n\npub fn run() {\n    take(1u8);\n}\n",
            "take",
        ),
    ] {
        let from = format!("pub fn {name}");
        let to = format!("pub const fn {name}");
        assert_compatible(old, &old.replace(&from, &to), caller, name, name);
        assert_blocking(old, &new.replace(&from, &to), caller, name, "mod");
    }
}

#[test]
fn review_regression_own_standard_macros_keep_const_addition_compatible() {
    assert_compatible(
        "pub fn take(value: u8) -> u8 { assert!(value > 0); value }\n",
        "pub const fn take(value: u8) -> u8 { assert!(value > 0); value }\n",
        "use crate::side::take;\n\npub fn run() {\n    take(1u8);\n}\n",
        "take",
        "take",
    );
    assert_compatible(
        "pub struct Counter;\nimpl Counter { pub fn len(&self) -> usize { assert!(true); 1 } }\n",
        "pub struct Counter;\nimpl Counter { pub const fn len(&self) -> usize { assert!(true); 1 } }\n",
        "use crate::side::Counter;\n\npub fn run(counter: &Counter) -> usize {\n    counter.len()\n}\n",
        "Counter.len",
        "len",
    );
}

#[test]
fn review_regression_batched_const_additions_do_not_block_hooks() {
    for old in [
        "pub fn f() { assert!(true); }\npub fn g() { assert!(true); }\n",
        "pub fn f() { assert!(true); g(); }\npub fn g() { assert!(true); }\n",
    ] {
        let repo = repo_with_change(
            old,
            &old.replace("pub fn", "pub const fn"),
            "use crate::side::{f, g};\n\npub fn run() {\n    f(); g();\n}\n",
        );
        let (code, stderr) = run_hook(&repo, "review", &[]);
        assert_eq!(code, Some(0), "review: {stderr}");
        let hook: serde_json::Value = serde_json::from_str(&stderr).unwrap();
        for name in ["f", "g"] {
            assert!(has_mod_compat(&hook, name), "{hook}");
            assert!(lists_caller(&hook, "impact_info", name), "{hook}");
        }
        assert!(api_entries(&hook, "mod").is_empty(), "{hook}");
        assert!(hook.get("impacts").is_none(), "{hook}");
        let (code, stderr) = run_hook(&repo, "impact", &[]);
        assert_eq!(code, Some(0), "impact: {stderr}");
    }
}

#[test]
fn review_regression_const_macro_headers_keep_normalization() {
    let body = " { assert!(value > 0); value }\n";
    for (old, new) in [
        (
            "pub fn take(mut value: u8) -> u8",
            "pub const fn take(value: u8) -> u8",
        ),
        (
            "pub fn take(value: u8,) -> u8",
            "pub const fn take(value: u8) -> u8",
        ),
        (
            "pub fn take(value: u8) -> u8",
            "pub const fn take(value: u8,) -> u8",
        ),
    ] {
        assert_compatible(
            &format!("{old}{body}"),
            &format!("{new}{body}"),
            "use crate::side::take;\n\npub fn run() {\n    take(1u8);\n}\n",
            "take",
            "take",
        );
    }
}

#[test]
fn review_regression_location_macros_keep_contract_changes_blocking() {
    for (old, header, size) in [
        (
            "pub struct S;\npub fn take(_: [u8; S::N]) {\n    impl S { const N: usize = line!() as usize; }\n}\n",
            "pub const\nfn take",
            3,
        ),
        (
            "pub struct S;\npub fn take(_: [u8; S::N]) { impl S { const N: usize = column!() as usize; } }\n",
            "pub const fn take",
            56,
        ),
        (
            "pub fn take(_: [u8; line!() as usize]) {}\n",
            "pub const\nfn take",
            1,
        ),
        (
            "pub fn take(_: [u8; column!() as usize]) {}\n",
            "pub const fn take",
            21,
        ),
        (
            "use core::line as here;\npub struct S;\npub fn take(_: [u8; S::N]) {\n    impl S { const N: usize = here!() as usize; }\n}\n",
            "pub const\nfn take",
            4,
        ),
        (
            "macro_rules! here { () => { line!() }; }\npub struct S;\npub fn take(_: [u8; S::N]) {\n    impl S { const N: usize = here!() as usize; }\n}\n",
            "pub const\nfn take",
            4,
        ),
    ] {
        let caller =
            format!("use crate::side::take;\n\npub fn run() {{\n    take([0u8; {size}]);\n}}\n");
        assert_blocking(
            old,
            &old.replace("pub fn take", header),
            &caller,
            "take",
            "mod",
        );
    }
    for (invocation, size) in [("line!()", 3), ("column!()", 31)] {
        let old = format!(
            "pub struct S;\npub fn take(_: [u8; S::N]) {{\n    impl S {{ const N: usize = {invocation} as usize; }}\n}}\n"
        );
        let caller =
            format!("use crate::side::take;\n\npub fn run() {{\n    take([0u8; {size}]);\n}}\n");
        assert_compatible(
            &old,
            &old.replace("pub fn take", "pub const fn take"),
            &caller,
            "take",
            "take",
        );
    }
}

/// 素の名前の呼び出し元を同名の別の宣言と区別できないときは、API が `mod_compat` でも
/// impacts を残す (別の impl の同名メソッド・cfg で分けた同名の非公開関数)。
#[test]
fn const_proof_keeps_impacts_when_the_callee_name_is_ambiguous() {
    for (old, new, caller, name, symbol) in [
        (
            "pub struct A;\npub struct B;\n\nimpl A {\n    pub fn len(&self) -> usize {\n        1\n    }\n}\n\nimpl B {\n    pub fn len(&self) -> usize {\n        2\n    }\n}\n".to_string(),
            "pub struct A;\npub struct B;\n\nimpl A {\n    pub const fn len(&self) -> usize {\n        1\n    }\n}\n\nimpl B {\n    pub fn len(&self) -> usize {\n        2\n    }\n}\n".to_string(),
            "use crate::side::A;\n\npub fn count(a: &A) -> usize {\n    a.len()\n}\n",
            "A.len",
            "len",
        ),
        (
            format!(
                "{SIDE_ENUM}#[cfg(unix)]\npub fn side_label(side: Side) -> &'static str{SIDE_BODY}\n#[cfg(not(unix))]\nfn side_label(side: Side) -> &'static str{SIDE_BODY}"
            ),
            format!(
                "{SIDE_ENUM}#[cfg(unix)]\npub const fn side_label(side: Side) -> &'static str{SIDE_BODY}\n#[cfg(not(unix))]\nfn side_label(side: Side) -> &'static str{SIDE_BODY}"
            ),
            SIDE_CALLER,
            "side_label",
            "side_label",
        ),
    ] {
        let repo = repo_with_change(&old, &new, caller);
        let (code, stderr) = run_hook(&repo, "review", &[]);
        assert_eq!(code, Some(1), "review: {stderr}");
        let hook: serde_json::Value = serde_json::from_str(&stderr).unwrap();
        assert!(has_mod_compat(&hook, name), "{hook}");
        assert!(lists_caller(&hook, "impacts", symbol), "{hook}");
        let (code, stderr) = run_hook(&repo, "impact", &[]);
        assert_eq!(code, Some(1), "impact: {stderr}");
    }
}

/// `--staged` は index の内容で判定し、作業ツリーの変更を証明に混ぜない。
#[test]
fn staged_review_judges_the_index_content() {
    let plain = side("pub fn side_label(side: Side) -> &'static str");
    let with_const = side("pub const fn side_label(side: Side) -> &'static str");
    let with_parameter = side("pub const fn side_label(side: Side, loud: bool) -> &'static str");

    let repo = base_repo(&plain, SIDE_CALLER);
    repo.write("src/side.rs", &with_const);
    repo.stage_all();
    repo.write("src/side.rs", &with_parameter);
    assert_compatible_hooks(&repo, &["--staged"], "side_label", "side_label");

    let repo = base_repo(&plain, SIDE_CALLER);
    repo.write("src/side.rs", &with_parameter);
    repo.stage_all();
    repo.write("src/side.rs", &with_const);
    assert_blocking_hooks(&repo, &["--staged"], "side_label", "mod");
}

/// 通常の review / context の出力と、stdin の diff を読む impact でも同じ判定になる。
#[test]
fn const_addition_is_compatible_in_review_context_and_stdin_impact() {
    let repo = repo_with_change(
        &side("pub fn side_label(side: Side) -> &'static str"),
        &side("pub const fn side_label(side: Side) -> &'static str"),
        SIDE_CALLER,
    );
    let calls = |callers: &serde_json::Value| {
        callers.as_array().is_some_and(|callers| {
            callers.iter().any(|caller| {
                caller["path"] == "src/caller.rs"
                    && caller["line"] == 3
                    && caller["symbols"]
                        .as_array()
                        .is_some_and(|symbols| symbols.iter().any(|s| s == "side_label"))
            })
        })
    };

    let review = repo.run_json("review", &["--git"]);
    assert!(
        review["api_changes"]["compatible_modified"]
            .as_array()
            .is_some_and(|changes| changes
                .iter()
                .any(|change| change["name"] == "side_label" && change["reason"] == REASON)),
        "{review}"
    );
    for result in [&review["impact"], &repo.run_json("context", &["--git"])] {
        let change = result["changes"]
            .as_array()
            .and_then(|changes| {
                changes
                    .iter()
                    .find(|change| change["path"] == "src/side.rs")
            })
            .unwrap_or_else(|| panic!("{result}"));
        assert!(calls(&change["informational_callers"]), "{change}");
        assert!(!calls(&change["impacted_callers"]), "{change}");
    }

    let diff = std::process::Command::new("git")
        .args(["diff", "--no-ext-diff"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(diff.status.success());
    let mut child = cargo_bin()
        .args(["impact", "--hook", "--dir"])
        .arg(repo.root())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&diff.stdout).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// impact エンジン自体も、const を足しただけの関数の呼び出し元を情報提供に分ける。
#[test]
fn impact_engine_moves_const_addition_callers_to_informational() {
    for (old, new, caller, symbol, compatible) in [
        (
            side("pub fn side_label(side: Side) -> &'static str"),
            side("pub const fn side_label(side: Side) -> &'static str"),
            SIDE_CALLER,
            "side_label",
            true,
        ),
        (
            COUNTER_OLD.to_string(),
            COUNTER_OLD.replace("pub fn len", "pub const fn len"),
            COUNTER_CALLER,
            "len",
            true,
        ),
        (
            side("pub const fn side_label(side: Side) -> &'static str"),
            side("pub fn side_label(side: Side) -> &'static str"),
            SIDE_CALLER,
            "side_label",
            false,
        ),
    ] {
        let repo = repo_with_change(&old, &new, caller);
        let diff_output = std::process::Command::new("git")
            .args(["diff", "--no-ext-diff"])
            .current_dir(repo.root())
            .output()
            .unwrap();
        assert!(diff_output.status.success());
        let diff = String::from_utf8(diff_output.stdout).unwrap();
        let result = astro_sight::service::AppService::new()
            .analyze_context(
                &diff,
                repo.root().to_str().unwrap(),
                &astro_sight::models::impact::ContextAnalysisOptions::default(),
            )
            .unwrap();
        let change = result
            .changes
            .iter()
            .find(|change| change.path == "src/side.rs")
            .unwrap();
        assert!(
            change
                .signature_changes
                .iter()
                .any(|signature| signature.name == symbol),
            "{new}: シグネチャの変更は痕跡として残す: {change:?}"
        );
        let calls = |callers: &[astro_sight::models::impact::ImpactedCaller]| {
            callers.iter().any(|caller| {
                caller.path == "src/caller.rs"
                    && caller.line == 3
                    && caller.symbols.iter().any(|s| s == symbol)
            })
        };
        assert_eq!(
            calls(&change.informational_callers),
            compatible,
            "{new}: {change:?}"
        );
        assert_eq!(
            calls(&change.impacted_callers),
            !compatible,
            "{new}: {change:?}"
        );
    }
}
