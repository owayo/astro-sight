//! 標準のマクロの引数の中の参照も、マクロの外と同じく所有元を確かめる (Issue #310)。

use super::support::*;

const CARGO: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

/// `files` をコミットし、`changes` に書き換えたときの context の出力。
fn context_with(
    cargo: &str,
    files: &[(&str, &str)],
    changes: &[(&str, &str)],
) -> serde_json::Value {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("Cargo.toml", cargo);
    for (path, content) in files {
        repo.write(path, content);
    }
    repo.init_git();
    repo.commit_all("initial");
    for (path, content) in changes {
        repo.write(path, content);
    }
    repo.run_json("context", &["--git"])
}

fn context(files: &[(&str, &str)], changes: &[(&str, &str)]) -> serde_json::Value {
    context_with(CARGO, files, changes)
}

/// `bucket` (`impacted_callers` など) の参照を `path:行` (1 始まり) で並べる。
fn callers(context: &serde_json::Value, bucket: &str) -> Vec<String> {
    let mut found: Vec<String> = context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| change[bucket].as_array().cloned().unwrap_or_default())
        .map(|caller| {
            format!(
                "{}:{}",
                caller["path"].as_str().unwrap(),
                caller["line"].as_u64().unwrap() + 1
            )
        })
        .collect();
    found.sort();
    found
}

/// どの振り分け先にも `path` の参照が無いか (所有元の証明で外れた)。
fn has_no_caller(context: &serde_json::Value, path: &str) -> bool {
    [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ]
    .iter()
    .all(|bucket| {
        callers(context, bucket)
            .iter()
            .all(|caller| !caller.starts_with(&format!("{path}:")))
    })
}

const KINDS: &str = "pub enum RefKind {\n    Definition,\n    Use,\n}\n";
/// 2 つの型に型引数を足す。
const INTERNAL_OLD: &str = "pub fn helper() {}\n\npub(crate) struct Definition {\n    range: (usize, usize),\n}\n\npub(crate) enum ModulePath {\n    Inline(Vec<String>),\n}\n";
const INTERNAL_NEW: &str = "pub fn helper() {}\n\npub(crate) struct Definition<T> {\n    range: (usize, usize),\n    extra: T,\n}\n\npub(crate) enum ModulePath<T> {\n    Inline(Vec<T>),\n}\n";
/// 自由関数 `helper` の引数を足す。
const HELPER_OLD: &str = "pub fn helper() {}\n";
const HELPER_NEW: &str = "pub fn helper(x: u8) {}\n";

const USER_MATCHES: &str = "use crate::kinds::RefKind;\n\npub fn is_definition(kind: &RefKind) -> bool {\n    matches!(kind, RefKind::Definition)\n}\n";

fn types_changed(files: &[(&str, &str)]) -> serde_json::Value {
    let mut all = vec![("src/internal.rs", INTERNAL_OLD)];
    all.extend_from_slice(files);
    context(&all, &[("src/internal.rs", INTERNAL_NEW)])
}

/// Issue の再現: `matches!` の中の別の enum の variant は所有元の証明で外れ、ファイル自身が
/// 宣言した同じ名前の型の経路は `match` で書いたときと同じく低確信度に回る。
#[test]
fn impact_rust_standard_macro_arguments_use_their_owners() {
    let context = types_changed(&[
        (
            "src/lib.rs",
            "pub mod kinds;\npub mod paths;\npub mod user;\nmod internal;\n",
        ),
        ("src/kinds.rs", KINDS),
        ("src/user.rs", USER_MATCHES),
        (
            "src/paths.rs",
            "enum ModulePath {\n    Crate,\n}\n\npub fn root() -> bool {\n    matches!(module(), ModulePath::Crate)\n}\n\nfn module() -> ModulePath {\n    ModulePath::Crate\n}\n",
        ),
    ]);
    assert!(
        callers(&context, "impacted_callers").is_empty(),
        "{context}"
    );
    assert!(has_no_caller(&context, "src/user.rs"), "{context}");
    assert_eq!(
        callers(&context, "low_confidence_callers"),
        ["src/paths.rs:10", "src/paths.rs:6"],
        "{context}"
    );
}

/// 同じ内容を `match` で書いた版と振り分けがそろう。
#[test]
fn impact_rust_standard_macro_arguments_match_the_match_expression() {
    let context = types_changed(&[
        (
            "src/lib.rs",
            "pub mod kinds;\npub mod paths;\npub mod user;\nmod internal;\n",
        ),
        ("src/kinds.rs", KINDS),
        (
            "src/user.rs",
            "use crate::kinds::RefKind;\n\npub fn is_definition(kind: &RefKind) -> bool {\n    match kind {\n        RefKind::Definition => true,\n        _ => false,\n    }\n}\n",
        ),
        (
            "src/paths.rs",
            "enum ModulePath {\n    Crate,\n}\n\npub fn root() -> bool {\n    match module() {\n        ModulePath::Crate => true,\n    }\n}\n\nfn module() -> ModulePath {\n    ModulePath::Crate\n}\n",
        ),
    ]);
    assert!(
        callers(&context, "impacted_callers").is_empty(),
        "{context}"
    );
    assert!(has_no_caller(&context, "src/user.rs"), "{context}");
    assert_eq!(
        callers(&context, "low_confidence_callers"),
        ["src/paths.rs:12", "src/paths.rs:7"],
        "{context}"
    );
}

/// 入れ子・ガード・or パターン・複数行・`assert_eq!` の書式の引数・`format!` の中も確かめる。
#[test]
fn impact_rust_nested_standard_macros_use_their_owners() {
    let context = types_changed(&[
        (
            "src/lib.rs",
            "pub mod kinds;\npub mod user;\nmod internal;\n",
        ),
        ("src/kinds.rs", KINDS),
        (
            "src/user.rs",
            "use crate::kinds::RefKind;\n\npub fn check(kind: &RefKind, flag: bool) {\n    assert!(matches!(kind, RefKind::Definition));\n    assert!(matches!(\n        kind,\n        RefKind::Definition | RefKind::Use if flag,\n    ));\n    assert_eq!(matches!(kind, RefKind::Definition), flag, \"{}\", matches!(kind, RefKind::Definition));\n    let _ = format!(\"{}\", matches!(kind, RefKind::Definition));\n}\n",
        ),
    ]);
    assert!(has_no_caller(&context, "src/user.rs"), "{context}");
}

/// 自由関数の変更: 標準のマクロの引数の中の、同じ名前のローカルな関数の呼び出しは外れる。
#[test]
fn impact_rust_standard_macro_arguments_prove_local_functions() {
    let context = context(
        &[
            ("src/lib.rs", "pub mod user;\nmod internal;\n"),
            ("src/internal.rs", HELPER_OLD),
            (
                "src/user.rs",
                "fn helper(x: u8) -> u8 {\n    x\n}\n\npub fn run() {\n    assert_eq!(helper(1), 1);\n    println!(\"{}\", helper(2));\n    let v = vec![helper(3); 2];\n    let _ = v;\n}\n",
            ),
        ],
        &[("src/internal.rs", HELPER_NEW)],
    );
    assert!(has_no_caller(&context, "src/user.rs"), "{context}");
}

/// 変えたものそのものを標準のマクロの引数で使う参照は残す。
#[test]
fn impact_rust_standard_macro_arguments_keep_real_references() {
    // import した型の variant
    let imported = types_changed(&[
        ("src/lib.rs", "pub mod user;\nmod internal;\n"),
        (
            "src/user.rs",
            "use crate::internal::ModulePath;\n\npub(crate) fn inline(path: &ModulePath) -> bool {\n    matches!(path, ModulePath::Inline(_))\n}\n",
        ),
    ]);
    assert!(
        callers(&imported, "impacted_callers").contains(&"src/user.rs:4".to_owned()),
        "{imported}"
    );
    // 経路で書いた型
    let qualified = types_changed(&[
        ("src/lib.rs", "pub mod user;\nmod internal;\n"),
        (
            "src/user.rs",
            "pub(crate) fn inline(path: &crate::internal::ModulePath) -> bool {\n    matches!(path, crate::internal::ModulePath::Inline(_))\n}\n",
        ),
    ]);
    assert!(
        callers(&qualified, "impacted_callers").contains(&"src/user.rs:2".to_owned()),
        "{qualified}"
    );
    // import した自由関数
    let function = context(
        &[
            ("src/lib.rs", "pub mod user;\nmod internal;\n"),
            ("src/internal.rs", HELPER_OLD),
            (
                "src/user.rs",
                "use crate::internal::helper;\n\npub fn run() {\n    assert!({ helper(); true });\n    println!(\"{:?}\", helper());\n    let v = vec![helper()];\n    let _ = v;\n}\n",
            ),
        ],
        &[("src/internal.rs", HELPER_NEW)],
    );
    assert_eq!(
        callers(&function, "impacted_callers"),
        ["src/user.rs:4", "src/user.rs:5", "src/user.rs:6"],
        "{function}"
    );
}

/// 写しに対応した参照も、引数の中の `use` (glob・一覧) と、書式の文字列の中の `let 名前` で
/// 振り分けを誤らない。
#[test]
fn impact_rust_standard_macro_arguments_keep_hidden_evidence() {
    let changed = |user: &str| {
        context(
            &[
                ("src/lib.rs", "mod internal;\npub mod user;\n"),
                ("src/internal.rs", HELPER_OLD),
                ("src/user.rs", user),
            ],
            &[("src/internal.rs", HELPER_NEW)],
        )
    };
    // 引数の中の glob と、`use` の一覧で持ち込んだ関数 (1 行と複数行)
    for (user, line) in [
        (
            "pub fn run() {\n    assert!({ use crate::internal::*; helper(); true });\n}\n",
            "src/user.rs:2",
        ),
        (
            "pub fn run() {\n    assert!({ use crate::internal::{helper}; helper(); true });\n}\n",
            "src/user.rs:2",
        ),
        (
            "pub fn run() {\n    assert!({\n        use crate::internal::*;\n        helper();\n        true\n    });\n}\n",
            "src/user.rs:4",
        ),
    ] {
        let context = changed(user);
        assert!(
            callers(&context, "impacted_callers").contains(&line.to_owned()),
            "{user}: {context}"
        );
    }
    // 書式の文字列の中の `let Config` は束縛ではない
    let alias = context(
        &[
            ("src/lib.rs", "mod internal;\npub mod user;\n"),
            ("src/internal.rs", "pub type Config = u8;\n"),
            (
                "src/user.rs",
                "use crate::internal::Config;\n\nfn only_u8(x: u8) -> u8 {\n    x\n}\n\npub fn run() {\n    println!(\"let Config = {}\", only_u8(Config::MAX));\n}\n",
            ),
        ],
        &[("src/internal.rs", "pub type Config = u16;\n")],
    );
    assert!(
        callers(&alias, "impacted_callers").contains(&"src/user.rs:8".to_owned()),
        "{alias}"
    );
    // 書式の文字列の中の `; use` は import ではない
    let format_text = changed(
        "use crate::internal::helper;\n\npub fn run() {\n    println!(\"; use {:?}\", helper());\n}\n",
    );
    assert!(
        callers(&format_text, "impacted_callers").contains(&"src/user.rs:4".to_owned()),
        "{format_text}"
    );
}

/// 写しの中でも、証明できない文の位置のマクロや属性が名前を足し得るスコープの参照は、今までどおり
/// 影響に残す。
#[test]
fn impact_rust_standard_macro_arguments_keep_uncertain_scopes() {
    let changed = |files: &[(&str, &str)]| {
        let mut all = vec![
            ("src/lib.rs", "mod internal;\nmod macros;\npub mod user;\n"),
            ("src/internal.rs", HELPER_OLD),
            (
                "src/macros.rs",
                "macro_rules! bring {\n    () => {\n        use crate::internal::*;\n    };\n}\n\npub(crate) use bring;\n",
            ),
        ];
        all.extend_from_slice(files);
        context(&all, &[("src/internal.rs", HELPER_NEW)])
    };
    // 引数の中の block で、自作のマクロが glob の `use` を足す
    let statement = changed(&[(
        "src/user.rs",
        "use crate::macros::bring;\n\npub fn run() {\n    assert!({ bring!(); helper(); true });\n}\n",
    )]);
    assert!(
        callers(&statement, "impacted_callers").contains(&"src/user.rs:4".to_owned()),
        "{statement}"
    );
    // 安全と確かめられない属性の付いたメソッド (属性のマクロは本体を書き換え得る)
    let attribute = changed(&[(
        "src/user.rs",
        "pub struct S;\n\nimpl S {\n    #[unknown_attribute]\n    pub fn run() {\n        assert!(helper());\n    }\n}\n",
    )]);
    assert!(
        callers(&attribute, "impacted_callers").contains(&"src/user.rs:6".to_owned()),
        "{attribute}"
    );
    // 引数の中の block の項目の属性のマクロ・import で差し替えた derive は、その block に `use` を
    // 足し得る
    // import で差し替えた `#[global_allocator]` (prelude の属性のマクロ) はファイルに `use` を足し得る
    let allocator = changed(&[(
        "src/user.rs",
        "use macro_support::global_allocator;\n\n#[global_allocator]\nstatic MARKER: () = ();\n\npub fn run() {\n    assert!(helper());\n}\n",
    )]);
    assert!(
        callers(&allocator, "impacted_callers").contains(&"src/user.rs:7".to_owned()),
        "{allocator}"
    );
    for user in [
        "use macro_support::bring;\n\npub fn run() {\n    assert!({ #[bring] struct Marker; helper(); true });\n}\n",
        "use macro_support::Debug;\n\npub fn run() {\n    assert!({ #[derive(Debug)] struct Marker; helper(); true });\n}\n",
    ] {
        let context = changed(&[("src/user.rs", user)]);
        assert!(
            callers(&context, "impacted_callers").contains(&"src/user.rs:4".to_owned()),
            "{user}: {context}"
        );
    }
}

/// 標準と証明できないマクロ (同じ名前の自作のマクロ・自作のマクロの中・依存で差し替え得る
/// 標準ライブラリ) の引数は、今までどおり影響に残す。
#[test]
fn impact_rust_unproven_macros_keep_arguments() {
    let impacted = |context: &serde_json::Value| {
        callers(context, "impacted_callers").contains(&"src/user.rs:4".to_owned())
    };
    // `#[macro_use]` で持ち込んだ自作の `matches!`
    let macro_use = types_changed(&[
        (
            "src/lib.rs",
            "#[macro_use]\nmod macros;\npub mod kinds;\npub mod user;\nmod internal;\n",
        ),
        (
            "src/macros.rs",
            "macro_rules! matches {\n    ($e:expr, $p:pat) => {\n        false\n    };\n}\n",
        ),
        ("src/kinds.rs", KINDS),
        ("src/user.rs", USER_MATCHES),
    ]);
    assert!(impacted(&macro_use), "{macro_use}");
    // 関数の中で定義した同じ名前のマクロ
    let local = types_changed(&[
        (
            "src/lib.rs",
            "pub mod kinds;\npub mod user;\nmod internal;\n",
        ),
        ("src/kinds.rs", KINDS),
        (
            "src/user.rs",
            "use crate::kinds::RefKind;\n\npub fn is_definition(kind: &RefKind) -> bool {\n    matches!(kind, RefKind::Definition) || {\n        macro_rules! matches {\n            ($e:expr, $p:pat) => {\n                false\n            };\n        }\n        matches!(kind, RefKind::Definition)\n    }\n}\n",
        ),
    ]);
    assert!(
        callers(&local, "impacted_callers").contains(&"src/user.rs:10".to_owned()),
        "{local}"
    );
    // 自作のマクロの引数の中の `matches!`
    let wrapped = types_changed(&[
        (
            "src/lib.rs",
            "pub mod kinds;\npub mod user;\nmod internal;\n",
        ),
        ("src/kinds.rs", KINDS),
        (
            "src/user.rs",
            "use crate::kinds::RefKind;\n\npub fn is_definition(kind: &RefKind) -> bool {\n    wrap!(matches!(kind, RefKind::Definition))\n}\n\nmacro_rules! wrap {\n    ($e:expr) => {\n        $e\n    };\n}\n",
        ),
    ]);
    assert!(impacted(&wrapped), "{wrapped}");
    // `core` という名前の依存は標準のマクロを差し替え得る
    let sysroot = context_with(
        &format!(
            "{CARGO}\n[dependencies]\ncore = {{ package = \"fake-core\", path = \"fake\" }}\n"
        ),
        &[
            (
                "src/lib.rs",
                "pub mod kinds;\npub mod user;\nmod internal;\n",
            ),
            ("src/kinds.rs", KINDS),
            ("src/user.rs", USER_MATCHES),
            ("src/internal.rs", INTERNAL_OLD),
        ],
        &[("src/internal.rs", INTERNAL_NEW)],
    );
    assert!(impacted(&sysroot), "{sysroot}");
    // インラインの module で import した、`matches` という名前の自作のマクロ (別名で再公開した
    // `macro_rules!` は名前の走査に現れない)。展開すると変えた関数を経路で呼ぶ。
    let inline_module = context(
        &[
            ("src/lib.rs", "mod internal;\nmod macros;\npub mod user;\n"),
            ("src/internal.rs", HELPER_OLD),
            (
                "src/macros.rs",
                "macro_rules! call_helper {\n    ($name:ident, $x:expr) => {\n        crate::internal::$name()\n    };\n}\n\npub(crate) use call_helper as matches;\n",
            ),
            (
                "src/user.rs",
                "pub mod inner {\n    use crate::macros::matches;\n\n    pub fn run() {\n        matches!(helper, 1);\n    }\n}\n",
            ),
        ],
        &[("src/internal.rs", HELPER_NEW)],
    );
    assert!(
        callers(&inline_module, "impacted_callers").contains(&"src/user.rs:5".to_owned()),
        "{inline_module}"
    );
}
