//! Rust の関数に `const` を足すだけの変更を互換変更に分けるテスト (Issue #113)。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

const REASON: &str = "rust_const_fn_added";

const SIDE_ENUM: &str = "#[derive(Clone, Copy)]\npub enum Side {\n    Left,\n    Right,\n}\n\n";
const SIDE_BODY: &str = " {\n    match side {\n        Side::Left => \"left\",\n        Side::Right => \"right\",\n    }\n}\n";
const SIDE_CALLER: &str = "use crate::side::{Side, side_label};\n\npub fn show() -> &'static str {\n    side_label(Side::Left)\n}\n";

/// `src/side.rs` を `old` から `new` に変えたときの API 差分。
fn api_changes(old: &str, new: &str, caller: &str) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"labels\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("src/lib.rs", "pub mod caller;\npub mod side;\n"),
            ("src/side.rs", old),
            ("src/caller.rs", caller),
        ],
        "initial",
    );
    fs::write(repo.join("src/side.rs"), new).expect("write changed side.rs");
    detect_api_changes_from_worktree(repo)
}

fn side(header: &str) -> String {
    format!("{SIDE_ENUM}{header}{SIDE_BODY}")
}

fn compatible_reason<'a>(api: &'a ApiChanges, name: &str) -> Option<&'a str> {
    api.compatible_modified
        .iter()
        .find(|change| change.name == name)
        .map(|change| change.reason.as_str())
}

fn is_modified(api: &ApiChanges, name: &str) -> bool {
    api.modified.iter().any(|change| change.name == name)
}

#[test]
fn adding_const_to_a_free_function_is_a_compatible_modification() {
    let api = api_changes(
        &side("pub fn side_label(side: Side) -> &'static str"),
        &side("pub const fn side_label(side: Side) -> &'static str"),
        SIDE_CALLER,
    );
    assert_eq!(
        compatible_reason(&api, "side_label"),
        Some(REASON),
        "{api:?}"
    );
    assert!(!is_modified(&api, "side_label"), "{api:?}");
}

#[test]
fn adding_const_to_an_inherent_method_is_a_compatible_modification() {
    let api = api_changes(
        "pub struct Counter {\n    items: [u8; 4],\n}\n\nimpl Counter {\n    pub fn len(&self) -> usize {\n        self.items.len()\n    }\n}\n",
        "pub struct Counter {\n    items: [u8; 4],\n}\n\nimpl Counter {\n    pub const fn len(&self) -> usize {\n        self.items.len()\n    }\n}\n",
        "use crate::side::Counter;\n\npub fn count(counter: &Counter) -> usize {\n    counter.len()\n}\n",
    );
    assert_eq!(
        compatible_reason(&api, "Counter.len"),
        Some(REASON),
        "{api:?}"
    );
    assert!(!is_modified(&api, "Counter.len"), "{api:?}");
}

/// 本体の書き換え・ヘッダの折り返し・束縛側の `mut` は、従来どおり呼び出し契約の違いに数えない。
/// `unsafe` や `extern "C"` が元からある関数も、`const` を足すだけなら同じ。
#[test]
fn const_addition_ignores_body_and_formatting_only_differences() {
    let plain = "pub fn side_label(side: Side) -> &'static str";
    for (old, new) in [
        (
            side(plain),
            side("pub const fn side_label(side: Side) -> &'static str")
                .replace("Side::Right => \"right\"", "Side::Right => \"RIGHT\""),
        ),
        (
            side(plain),
            side("pub const fn side_label(\n    side: Side,\n) -> &'static str"),
        ),
        (
            side(plain),
            side("pub const fn side_label(mut side: Side) -> &'static str"),
        ),
        (
            side(&format!("/// 左右の名前\n#[inline]\n{plain}")),
            side("/// 左右の名前\n#[inline]\npub const fn side_label(side: Side) -> &'static str"),
        ),
        (
            side("pub unsafe fn side_label(side: Side) -> &'static str"),
            side("pub const unsafe fn side_label(side: Side) -> &'static str"),
        ),
        (
            side("pub extern \"C\" fn side_label(side: Side) -> &'static str"),
            side("pub const extern \"C\" fn side_label(side: Side) -> &'static str"),
        ),
    ] {
        let caller = if old.contains("unsafe fn") {
            SIDE_CALLER.replace(
                "    side_label(Side::Left)",
                "    unsafe { side_label(Side::Left) }",
            )
        } else {
            SIDE_CALLER.to_string()
        };
        let api = api_changes(&old, &new, &caller);
        assert_eq!(
            compatible_reason(&api, "side_label"),
            Some(REASON),
            "{new}: {api:?}"
        );
        assert!(!is_modified(&api, "side_label"), "{new}: {api:?}");
    }
}

/// 同じファイルの複数の関数・別の impl の同名メソッドも、それぞれ証明する。出力は宣言の順。
#[test]
fn const_addition_proves_every_function_of_the_same_file() {
    let old = format!(
        "{SIDE_ENUM}pub fn side_label(side: Side) -> &'static str{SIDE_BODY}\npub fn side_code(side: Side) -> u8 {{\n    side as u8\n}}\n\npub struct A;\npub struct B;\n\nimpl A {{\n    pub fn len(&self) -> usize {{\n        1\n    }}\n}}\n\nimpl B {{\n    pub fn len(&self) -> usize {{\n        2\n    }}\n}}\n"
    );
    let new = old.replace("pub fn", "pub const fn");
    let api = api_changes(
        &old,
        &new,
        "use crate::side::{A, B, Side, side_code, side_label};\n\npub fn show() -> (&'static str, u8, usize) {\n    (side_label(Side::Left), side_code(Side::Right), A.len() + B.len())\n}\n",
    );
    let names: Vec<_> = api
        .compatible_modified
        .iter()
        .map(|change| (change.name.as_str(), change.reason.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("side_label", REASON),
            ("side_code", REASON),
            ("A.len", REASON),
            ("B.len", REASON)
        ],
        "{api:?}"
    );
    assert!(api.modified.is_empty(), "{api:?}");
}

/// `const` を外す変更と、`const` と同時にほかの部分も変える変更は従来どおり blocking。
#[test]
fn const_removal_and_other_simultaneous_changes_stay_api_mod() {
    let plain = "pub fn side_label(side: Side) -> &'static str";
    for (label, old, new) in [
        (
            "remove const",
            "pub const fn side_label(side: Side) -> &'static str",
            plain,
        ),
        (
            "add parameter",
            plain,
            "pub const fn side_label(side: Side, loud: bool) -> &'static str",
        ),
        (
            "rename parameter",
            plain,
            "pub const fn side_label(_side: Side) -> &'static str",
        ),
        (
            "change return type",
            plain,
            "pub const fn side_label(side: Side) -> &'static [u8]",
        ),
        (
            "add generic parameter",
            plain,
            "pub const fn side_label<T>(side: Side) -> &'static str",
        ),
        (
            "add unsafe",
            plain,
            "pub const unsafe fn side_label(side: Side) -> &'static str",
        ),
        (
            "add async",
            plain,
            "pub const async fn side_label(side: Side) -> &'static str",
        ),
        (
            "const on an async function",
            "pub async fn side_label(side: Side) -> &'static str",
            "pub const async fn side_label(side: Side) -> &'static str",
        ),
        (
            "add extern",
            plain,
            "pub const extern \"C\" fn side_label(side: Side) -> &'static str",
        ),
        (
            "change ABI",
            "pub extern \"C\" fn side_label(side: Side) -> &'static str",
            "pub const extern \"system\" fn side_label(side: Side) -> &'static str",
        ),
        (
            "modifier order rejected by rustc",
            "pub unsafe fn side_label(side: Side) -> &'static str",
            "pub unsafe const fn side_label(side: Side) -> &'static str",
        ),
        (
            "unknown attribute",
            "#[instrument]\npub fn side_label(side: Side) -> &'static str",
            "#[instrument]\npub const fn side_label(side: Side) -> &'static str",
        ),
        (
            "attribute added",
            plain,
            "#[must_use]\npub const fn side_label(side: Side) -> &'static str",
        ),
    ] {
        let api = api_changes(&side(old), &side(new), SIDE_CALLER);
        assert!(is_modified(&api, "side_label"), "{label}: {api:?}");
        assert_eq!(
            compatible_reason(&api, "side_label"),
            None,
            "{label}: {api:?}"
        );
    }
}

#[test]
fn const_with_narrowed_visibility_stays_a_removal() {
    let api = api_changes(
        &side("pub fn side_label(side: Side) -> &'static str"),
        &side("pub(crate) const fn side_label(side: Side) -> &'static str"),
        SIDE_CALLER,
    );
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "side_label"),
        "{api:?}"
    );
    assert_eq!(compatible_reason(&api, "side_label"), None, "{api:?}");
}

/// メソッドの外側 (impl のヘッダ・同じファイルの import) や、ヘッダに現れる名前の宣言が
/// 同時に変わると、呼び出し契約が変わりうるので証明しない。trait のメソッドは const に
/// できないので対象外。どれも従来どおり api.mod に残る。
#[test]
fn const_method_proof_requires_unchanged_context() {
    for (label, old, new, caller, name) in [
        (
            "impl bounds",
            "pub struct Wrap<T>(pub T);\n\nimpl<T: Copy> Wrap<T> {\n    pub fn get(&self) -> u8 {\n        0\n    }\n}\n",
            "pub struct Wrap<T>(pub T);\n\nimpl<T: Clone> Wrap<T> {\n    pub const fn get(&self) -> u8 {\n        0\n    }\n}\n",
            "use crate::side::Wrap;\n\npub fn read(w: &Wrap<u8>) -> u8 {\n    w.get()\n}\n",
            "Wrap.get",
        ),
        (
            "import",
            "pub mod a {\n    pub struct Kind;\n}\npub mod b {\n    pub struct Kind;\n}\nuse a::Kind;\n\npub fn take(_kind: &Kind) -> u8 {\n    0\n}\n",
            "pub mod a {\n    pub struct Kind;\n}\npub mod b {\n    pub struct Kind;\n}\nuse b::Kind;\n\npub const fn take(_kind: &Kind) -> u8 {\n    0\n}\n",
            "use crate::side::{a, take};\n\npub fn read() -> u8 {\n    take(&a::Kind)\n}\n",
            "take",
        ),
        (
            "trait default method",
            "pub trait Label {\n    fn label(&self) -> u8 {\n        0\n    }\n}\n",
            "pub trait Label {\n    const fn label(&self) -> u8 {\n        0\n    }\n}\n",
            "use crate::side::Label;\n\npub fn read<L: Label>(l: &L) -> u8 {\n    l.label()\n}\n",
            "Label.label",
        ),
        // ヘッダの字面は `const` 以外同じでも、`Arg` の実体が変わるので `take(1u8)` は壊れる。
        (
            "private type alias",
            "type Arg = u8;\n\npub fn take(_value: Arg) -> u8 {\n    0\n}\n",
            "type Arg = u16;\n\npub const fn take(_value: Arg) -> u8 {\n    0\n}\n",
            "use crate::side::take;\n\npub fn read() -> u8 {\n    take(1u8)\n}\n",
            "take",
        ),
        (
            "array length constant",
            "const LEN: usize = 4;\n\npub fn take(_value: [u8; LEN]) -> u8 {\n    0\n}\n",
            "const LEN: usize = 8;\n\npub const fn take(_value: [u8; LEN]) -> u8 {\n    0\n}\n",
            "use crate::side::take;\n\npub fn read() -> u8 {\n    take([0; 4])\n}\n",
            "take",
        ),
        (
            "use alias of a changed alias",
            "type Original = u8;\nuse self::Original as Arg;\n\npub fn take(_value: Arg) -> u8 {\n    0\n}\n",
            "type Original = u16;\nuse self::Original as Arg;\n\npub const fn take(_value: Arg) -> u8 {\n    0\n}\n",
            "use crate::side::take;\n\npub fn read() -> u8 {\n    take(1u8)\n}\n",
            "take",
        ),
        (
            "impl inside a function body",
            "pub struct S;\npub trait T {\n    type A;\n}\n\nfn install() {\n    impl T for S {\n        type A = u8;\n    }\n}\n\npub fn take(_value: <S as T>::A) -> u8 {\n    0\n}\n",
            "pub struct S;\npub trait T {\n    type A;\n}\n\nfn install() {\n    impl T for S {\n        type A = u16;\n    }\n}\n\npub const fn take(_value: <S as T>::A) -> u8 {\n    0\n}\n",
            "use crate::side::take;\n\npub fn read() -> u8 {\n    take(1u8)\n}\n",
            "take",
        ),
        (
            "opaque return type with a changed body",
            "pub fn make() -> impl Copy {\n    0u8\n}\n",
            "pub const fn make() -> impl Copy {\n    core::marker::PhantomData::<*const ()>\n}\n",
            "fn need_send<T: Send>(_: T) {}\n\npub fn read() {\n    need_send(crate::side::make());\n}\n",
            "make",
        ),
    ] {
        let api = api_changes(old, new, caller);
        assert!(
            api.compatible_modified.is_empty(),
            "{label}: const の証明に使ってはならない: {api:?}"
        );
        assert!(is_modified(&api, name), "{label}: {api:?}");
    }
}
