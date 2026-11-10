//! Rust の inherent impl の見出し (対象の型・型の引数の境界・`where`) の変更 (Issue #307)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

/// `src/lib.rs` を `old` でコミットし、`new` に書き換えたときの API の差分。
fn api_changes(old: &str, new: &str) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("Cargo.toml", CARGO), ("src/lib.rs", old)], "base");
    fs::write(repo.join("src/lib.rs"), new).expect("write");
    detect_api_changes_from_worktree(repo)
}

/// `api_changes` の `name` の区分 (mod・mod_compat・none ほか)。
fn bucket(api: &crate::models::review::ApiChanges, name: &str) -> &'static str {
    if api.modified.iter().any(|change| change.name == name) {
        "mod"
    } else if api
        .compatible_modified
        .iter()
        .any(|change| change.name == name && change.reason == "rust_impl_header_compatible")
    {
        "mod_compat"
    } else if api
        .compatible_modified
        .iter()
        .any(|change| change.name == name)
    {
        "mod_compat_other"
    } else if api
        .const_value_changes
        .iter()
        .any(|change| change.name == name)
    {
        "const_value"
    } else if api.removed.iter().any(|symbol| symbol.name == name)
        || api.removed_dead.iter().any(|symbol| symbol.name == name)
    {
        "rm"
    } else if api.moved.iter().any(|moved| moved.name == name) {
        "moved"
    } else {
        "none"
    }
}

/// `pub struct S<T>(pub T);` の inherent impl `header` に `body` を置いたソース。
fn source(header: &str, body: &str) -> String {
    format!("pub struct S<T>(pub T);\n\n{header} {{\n{body}\n}}\n")
}

const METHOD: &str = "    pub fn f(&self) {}";

/// impl の型の引数に境界を足す (`impl<T> S<T>` → `impl<T: Copy> S<T>`) と、境界を満たさない型ではメソッドを
/// 呼べなくなるので blocking な api.mod。`where` で足しても、対象の型を狭めても同じ。見出しを添えて示す。
#[test]
fn stronger_impl_headers_are_blocking() {
    for (old, new) in [
        ("impl<T> S<T>", "impl<T: Copy> S<T>"),
        ("impl<T> S<T>", "impl<T> S<T>\nwhere\n    T: Copy,"),
        ("impl<T: Copy> S<T>", "impl<T: Copy + Send> S<T>"),
        ("impl<T> S<T>", "impl S<u8>"),
    ] {
        let api = api_changes(&source(old, METHOD), &source(new, METHOD));
        assert_eq!(bucket(&api, "S.f"), "mod", "{old} => {new}: {api:?}");
        let change = api
            .modified
            .iter()
            .find(|change| change.name == "S.f")
            .expect("modified");
        assert!(
            change
                .new_signature
                .as_deref()
                .is_some_and(|signature| signature.starts_with("impl")),
            "{change:?}"
        );
    }
}

/// 境界を緩めるだけ (`impl<T: Copy> S<T>` → `impl<T> S<T>`) なら、今の利用を壊さないので互換。
#[test]
fn weaker_impl_headers_are_compatible() {
    for (old, new) in [
        ("impl<T: Copy> S<T>", "impl<T> S<T>"),
        (
            "impl<T> S<T>\nwhere\n    T: Copy + Send,",
            "impl<T: Copy> S<T>",
        ),
    ] {
        let api = api_changes(&source(old, METHOD), &source(new, METHOD));
        assert_eq!(bucket(&api, "S.f"), "mod_compat", "{old} => {new}: {api:?}");
    }
}

/// 型の引数の改名・`where` への書き換え・整形・コメントは、使える型を変えないので報告しない。
#[test]
fn equivalent_impl_headers_are_not_reported() {
    for (old, new) in [
        ("impl<T: Copy> S<T>", "impl<U> S<U>\nwhere\n    U: Copy,"),
        ("impl<T> S<T>", "impl<T>\n    S<T> /* same */"),
    ] {
        let api = api_changes(&source(old, METHOD), &source(new, METHOD));
        assert!(
            matches!(bucket(&api, "S.f"), "none" | "moved"),
            "{old} => {new}: {api:?}"
        );
        assert!(
            !api.modified.iter().any(|change| change.name == "S.f"),
            "{old} => {new}: {api:?}"
        );
    }
}

/// 関連定数も、囲む impl の見出しを強めれば blocking (値だけの変更にしない)。
#[test]
fn associated_constants_follow_impl_headers() {
    let body = "    pub const N: usize = 1;";
    let api = api_changes(
        &source("impl<T> S<T>", body),
        &source("impl<T: Copy> S<T>", body),
    );
    assert_eq!(bucket(&api, "S.N"), "mod", "{api:?}");
}

/// trait の impl のメソッドは別の API 項目にしないので、見出しの変更も報告しない。
#[test]
fn trait_impl_headers_are_not_api_items() {
    let head = "pub struct S<T>(pub T);\npub trait Tr {\n    fn f(&self);\n}\n\n";
    let body = " {\n    fn f(&self) {}\n}\n";
    let api = api_changes(
        &format!("{head}impl<T> Tr for S<T>{body}"),
        &format!("{head}impl<T: Copy> Tr for S<T>{body}"),
    );
    assert!(api.modified.is_empty(), "{api:?}");
    assert!(api.compatible_modified.is_empty(), "{api:?}");
}

/// 読めない見出し (型の引数の属性) に変えたら、比べられないので blocking に残す。
#[test]
fn unreadable_impl_headers_stay_blocking() {
    let api = api_changes(
        &source("impl<T> S<T>", METHOD),
        &source("impl<#[cfg(all())] T> S<T>", METHOD),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 寿命の境界を外すのも緩めるだけ (互換)。値の位置の const の引数の改名は報告しない。型の位置に書いた const の
/// 引数の改名は、同じ名前の型と区別できないので報告する。
#[test]
fn lifetime_bounds_and_const_parameters_follow_impl_rules() {
    let api = api_changes(
        &source("impl<T: 'static> S<T>", METHOD),
        &source("impl<T> S<T>", METHOD),
    );
    assert_eq!(bucket(&api, "S.f"), "mod_compat", "{api:?}");
    let head = "pub struct S<const N: usize>(pub [u8; N]);\n\n";
    let api = api_changes(
        &format!("{head}impl<const N: usize> S<{{ N }}> {{\n{METHOD}\n}}\n"),
        &format!("{head}impl<const M: usize> S<{{ M }}> {{\n{METHOD}\n}}\n"),
    );
    assert!(api.modified.is_empty(), "{api:?}");
    assert!(api.compatible_modified.is_empty(), "{api:?}");
    let api = api_changes(
        &format!("{head}impl<const N: usize> S<N> {{\n{METHOD}\n}}\n"),
        &format!("{head}impl<const M: usize> S<M> {{\n{METHOD}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 1 つの impl を複数の対象の型の impl に分けると、どれとどれを比べるか決められないので blocking に残す。
#[test]
fn split_impls_stay_blocking() {
    let api = api_changes(
        &source("impl<T> S<T>", METHOD),
        &format!(
            "pub struct S<T>(pub T);\n\nimpl S<u8> {{\n{METHOD}\n}}\n\nimpl S<u16> {{\n{METHOD}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// impl を別のファイルへ移すときに境界を強めても、見出しを比べて blocking にする。
#[test]
fn moved_impls_compare_their_headers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod methods;\npub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f(&self) {}\n}\n",
            ),
            ("src/methods.rs", ""),
        ],
        "base",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod methods;\npub struct S<T>(pub T);\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/methods.rs"),
        "use crate::S;\n\nimpl<T: Copy> S<T> {\n    pub fn f(&self) {}\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// impl の型の引数の並べ替えと、`self::` の有無は報告しない。別の module の同じ名前の型への付け替えと、const の
/// 引数と同じ名前の trait の境界の変更は報告する。
#[test]
fn impl_parameters_follow_the_self_type() {
    let api = api_changes(
        "pub struct S<T, U>(pub T, pub U);\n\nimpl<T, U> S<T, U> {\n    pub fn f(&self) {}\n}\n",
        "pub struct S<T, U>(pub T, pub U);\n\nimpl<U, T> S<T, U> {\n    pub fn f(&self) {}\n}\n",
    );
    assert!(api.modified.is_empty(), "{api:?}");
    assert!(api.compatible_modified.is_empty(), "{api:?}");
    let api = api_changes(
        &source("impl<T> S<T>", METHOD),
        &source("impl<T> self::S<T>", METHOD),
    );
    assert!(api.modified.is_empty(), "{api:?}");
    assert!(api.removed.is_empty(), "{api:?}");
    let head = "pub mod a {\n    pub struct S;\n}\npub mod b {\n    pub struct S;\n}\n\n";
    let api = api_changes(
        &format!("{head}impl a::S {{\n{METHOD}\n}}\n"),
        &format!("{head}impl b::S {{\n{METHOD}\n}}\n"),
    );
    // 経路で修飾した型の impl のメソッドの名前には型が入らない (`f`。Issue #322)。組にして変更を消さない。
    assert_ne!(bucket(&api, "f"), "none", "{api:?}");
    let head = "pub struct S<T, const N: usize>(pub T);\npub trait A {}\npub trait B {}\nimpl A for String {}\n\n";
    let api = api_changes(
        &format!("{head}impl<T: A, const A: usize> S<T, {{ A }}> {{\n{METHOD}\n}}\n"),
        &format!("{head}impl<T: B, const B: usize> S<T, {{ B }}> {{\n{METHOD}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 別のファイルへ移しながら impl を複数の対象の型の impl に分けると、blocking に残す。
#[test]
fn moved_split_impls_stay_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod methods;\npub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f(&self) {}\n}\n",
            ),
            ("src/methods.rs", ""),
        ],
        "base",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod methods;\npub struct S<T>(pub T);\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/methods.rs"),
        "use crate::S;\n\nimpl S<u8> {\n    pub fn f(&self) {}\n}\n\nimpl S<u16> {\n    pub fn f(&self) {}\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 型ごと別のファイルへ移し、メソッドの signature も変えたとき、囲む impl の見出しを強めたなら、中の利用を
/// 同じ差分で直しても blocking に残す。
#[test]
fn moved_members_with_stronger_impl_headers_stay_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\npub use a::S;\n\npub fn call(s: &S<u8>) {\n    S::<u8>::f(s);\n}\n",
            ),
            (
                "src/a.rs",
                "pub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f(&self) {}\n}\n",
            ),
            ("src/b.rs", ""),
        ],
        "base",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod a;\npub mod b;\npub use b::S;\n\npub fn call(s: &S<u8>) {\n    S::<u8>::f(s, 0);\n}\n",
    )
    .expect("write");
    fs::write(repo.join("src/a.rs"), "").expect("write");
    fs::write(
        repo.join("src/b.rs"),
        "pub struct S<T>(pub T);\n\nimpl<T: Copy> S<T> {\n    pub fn f(&self, _: u8) {}\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// const の引数と同じ名前の型が見えるなら、型の位置の名前は型 (対象の型の変更を消さない)。型の位置に書いた
/// const の引数の改名は、同じ名前の型と区別できないので報告する。関連型の等式にだけ現れる引数の並べ替えは報告
/// しない。
#[test]
fn const_parameters_and_associated_equalities_follow_rust_rules() {
    let head = "pub struct A {}\npub struct B {}\npub struct S<T, const N: usize>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<const A: usize> S<A, {{ A }}> {{\n{METHOD}\n}}\n"),
        &format!("{head}impl<const B: usize> S<B, {{ B }}> {{\n{METHOD}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "pub trait Bound<const N: usize> {}\npub struct S<T, const N: usize>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<T: Bound<N>, const N: usize> S<T, N> {{\n{METHOD}\n}}\n"),
        &format!("{head}impl<T: Bound<M>, const M: usize> S<T, M> {{\n{METHOD}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "pub trait Tr {\n    type A;\n    type B;\n}\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<T, U, V> S<T>\nwhere\n    T: Tr<A = U, B = V>,\n{{\n{METHOD}\n}}\n"),
        &format!("{head}impl<T, V, U> S<T>\nwhere\n    T: Tr<A = U, B = V>,\n{{\n{METHOD}\n}}\n"),
    );
    assert!(api.modified.is_empty(), "{api:?}");
}

/// 移動先で `#[cfg]` の分岐に分けながら境界を強めても blocking。同じ名前の特殊化 impl の 1 つを修飾して
/// 移すだけなら報告しない。
#[test]
fn moved_impls_into_branches_and_partial_moves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod methods;\npub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f(&self) {}\n}\n",
            ),
            ("src/methods.rs", ""),
        ],
        "base",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod methods;\npub struct S<T>(pub T);\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/methods.rs"),
        "use crate::S;\n\n#[cfg(unix)]\nimpl<T: Copy> S<T> {\n    pub fn f(&self) {}\n}\n\n#[cfg(not(unix))]\nimpl<T: Copy> S<T> {\n    pub fn f(&self) {}\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod methods;\npub struct S<T>(pub T);\n\nimpl S<u8> {\n    pub fn f(&self) {}\n}\n\nimpl S<u16> {\n    pub fn f(&self) {}\n}\n",
            ),
            ("src/methods.rs", ""),
        ],
        "base",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod methods;\npub struct S<T>(pub T);\n\nimpl S<u8> {\n    pub fn f(&self) {}\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/methods.rs"),
        "impl crate::S<u16> {\n    pub fn f(&self) {}\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert!(api.modified.is_empty(), "{api:?}");
}

/// `src/lib.rs` を `old_lib` (と空の `src/methods.rs`) でコミットし、`new_lib` と `new_methods` に書き換えた
/// ときの API の差分。
fn moved_changes(
    old_lib: &str,
    new_lib: &str,
    new_methods: &str,
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", old_lib),
            ("src/methods.rs", ""),
        ],
        "base",
    );
    fs::write(repo.join("src/lib.rs"), new_lib).expect("write");
    fs::write(repo.join("src/methods.rs"), new_methods).expect("write");
    detect_api_changes_from_worktree(repo)
}

/// prelude の型と同じ名前の const の引数でも、型の位置の型 (`Vec<u8>`) は置き換えない。関連型の等式の境界の
/// 並べ替えは報告しない。
#[test]
fn prelude_types_and_reordered_equalities_follow_rust_rules() {
    let head = "pub struct S<T, const N: usize>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<const Vec: usize> S<Vec<u8>, {{ Vec }}> {{\n{METHOD}\n}}\n"),
        &format!("{head}impl<const Box: usize> S<Box<u8>, {{ Box }}> {{\n{METHOD}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head =
        "pub trait A {\n    type X;\n}\npub trait B {\n    type X;\n}\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T, U, V> S<T>\nwhere\n    T: A<X = U> + B<X = V>,\n{{\n{METHOD}\n}}\n"
        ),
        &format!(
            "{head}impl<T, U, V> S<T>\nwhere\n    T: B<X = V> + A<X = U>,\n{{\n{METHOD}\n}}\n"
        ),
    );
    assert!(api.modified.is_empty(), "{api:?}");
}

/// `#[cfg]` の分岐の impl をそのまま移すだけ、同じ名前の特殊化 impl の 1 つを型の引数の名前を変えて移すだけ
/// なら報告しない。
#[test]
fn moved_branches_and_renamed_specializations_are_not_reported() {
    let head = "pub mod methods;\npub struct S<T, const K: usize>(pub T);\n\n";
    let impls = "#[cfg(unix)]\nimpl<T: Copy> S<T, 0> {\n    pub fn f(&self) {}\n}\n\n#[cfg(not(unix))]\nimpl<T: Clone> S<T, 0> {\n    pub fn f(&self) {}\n}\n\nimpl<T> S<T, 1> {\n    pub fn f(&self, _: u8) {}\n}\n";
    let api = moved_changes(
        &format!("{head}{impls}"),
        head,
        &format!("use crate::S;\n\n{impls}"),
    );
    assert!(api.modified.is_empty(), "{api:?}");
    let head = "pub mod methods;\npub struct S<T, U>(pub T, pub U);\n\nimpl<T> S<T, u8> {\n    pub fn f(&self) {}\n}\n";
    let api = moved_changes(
        &format!("{head}\nimpl<T> S<T, u16> {{\n    pub fn f(&self) {{}}\n}}\n"),
        head,
        "impl<V> crate::S<V, u16> {\n    pub fn f(&self) {}\n}\n",
    );
    assert!(api.modified.is_empty(), "{api:?}");
}

/// impl の型の引数の改名は、メソッドの signature も付け直して比べる (改名だけなら報告せず、見出しを緩めた
/// だけなら互換)。対象の型の中の並べ替えでメソッドの引数の型が変われば報告する。
#[test]
fn member_signatures_follow_impl_parameter_names() {
    let renamed = "pub struct S<T>(pub T);\n\nimpl<U> S<U> {\n    pub fn get(&self) -> &U {\n        &self.0\n    }\n}\n";
    let api = api_changes(
        "pub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn get(&self) -> &T {\n        &self.0\n    }\n}\n",
        renamed,
    );
    assert_eq!(bucket(&api, "S.get"), "none", "{api:?}");
    assert!(api.added.is_empty(), "{api:?}");
    let api = api_changes(
        "pub struct S<T>(pub T);\n\nimpl<T: Copy> S<T> {\n    pub fn get(&self) -> &T {\n        &self.0\n    }\n}\n",
        renamed,
    );
    assert_eq!(bucket(&api, "S.get"), "mod_compat", "{api:?}");
    let api = api_changes(
        "pub struct S<T, U>(pub T, pub U);\n\nimpl<T, U> S<T, U> {\n    pub fn f(&self, _: T) {}\n}\n",
        "pub struct S<T, U>(pub T, pub U);\n\nimpl<T, U> S<U, T> {\n    pub fn f(&self, _: T) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 境界の先頭の trait の名前 (Rust 2024 の prelude の `Future`・`IntoFuture` を含む) は、同じ名前の const の
/// 引数として置き換えない。番号の決まらない寿命どうしは境界の中の形で区別する (`T: 'a + 'b` の並べ替えを
/// 報告しない)。
#[test]
fn bound_traits_and_tied_lifetimes_follow_rust_rules() {
    let head = "pub struct S<T, const N: usize>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T: IntoFuture, const IntoFuture: usize> S<T, {{ IntoFuture }}> {{\n{METHOD}\n}}\n"
        ),
        &format!(
            "{head}impl<T: Future, const Future: usize> S<T, {{ Future }}> {{\n{METHOD}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let api = api_changes(
        &source(
            "impl<'a, 'b, T> S<T>\nwhere\n    T: 'a + 'b,\n    'a: 'b,",
            METHOD,
        ),
        &source(
            "impl<'a, 'b, T> S<T>\nwhere\n    T: 'b + 'a,\n    'a: 'b,",
            METHOD,
        ),
    );
    assert!(api.modified.is_empty(), "{api:?}");
}

/// 移動とともに `#[cfg]` の書き方だけを変えた分岐も、同じ設定で成り立つ分岐どうしで比べる。
#[test]
fn moved_branches_with_rewritten_conditions_are_not_reported() {
    let head = "pub mod methods;\npub struct S<T, const K: usize>(pub T);\n\n";
    let impls = |condition: &str| {
        format!(
            "#[cfg({condition})]\nimpl<T: Copy> S<T, 0> {{\n    pub fn f(&self) {{}}\n}}\n\n#[cfg(not(all(unix, debug_assertions)))]\nimpl<T: Clone> S<T, 0> {{\n    pub fn f(&self) {{}}\n}}\n\nimpl<T> S<T, 1> {{\n    pub fn f(&self, _: u8) {{}}\n}}\n"
        )
    };
    for rewritten in [
        "all( unix , debug_assertions )",
        "all(debug_assertions, unix)",
    ] {
        let api = moved_changes(
            &format!("{head}{}", impls("all(unix, debug_assertions)")),
            head,
            &format!("use crate::S;\n\n{}", impls(rewritten)),
        );
        assert!(api.modified.is_empty(), "{rewritten}: {api:?}");
    }
}

/// 見出しの型の引数の改名とともに、関連定数の値を変えれば値の変更、引数のパターンの `mut` だけなら報告せず、
/// 境界を削っただけなら (寿命の番号が入れ替わっても) 互換。寿命の関係から同じになることは確かめない
/// (`T: 'a + 'b, 'a: 'b` から `T: 'b` で `'a` が使われなくなれば、使われる引数の数が違うので blocking)。
#[test]
fn renamed_members_keep_values_patterns_and_bindings() {
    let api = api_changes(
        "pub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub const N: usize = 1;\n}\n",
        "pub struct S<T>(pub T);\n\nimpl<U> S<U> {\n    pub const N: usize = 2;\n}\n",
    );
    assert_eq!(bucket(&api, "S.N"), "const_value", "{api:?}");
    let api = api_changes(
        "pub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f(&self, x: u8) {}\n}\n",
        "pub struct S<T>(pub T);\n\nimpl<U> S<U> {\n    pub fn f(&self, mut x: u8) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
    let api = api_changes(
        &source(
            "impl<'a, 'b, T> S<T>\nwhere\n    T: 'a + 'b,\n    'a: 'b,",
            METHOD,
        ),
        &source(
            "impl<'a, 'b, T> S<T>\nwhere\n    T: 'b,\n    'a: 'b,",
            METHOD,
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "pub struct S<T, U>(pub T, pub U);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<'a, 'b, T, U> S<T, U>\nwhere\n    T: 'a + 'b,\n    U: 'a,\n    'a: 'b,\n{{\n{METHOD}\n}}\n"
        ),
        &format!(
            "{head}impl<'a, 'b, T, U> S<T, U>\nwhere\n    T: 'b,\n    U: 'a,\n    'a: 'b,\n{{\n{METHOD}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod_compat", "{api:?}");
}

/// 型ごと移したメンバーは、見出しの型の引数の改名だけに見えても、名指す名前の束縛を比べずに互換にしない。
#[test]
fn moved_renamed_members_keep_binding_checks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\npub use a::S;\npub struct A;\npub struct B;\n",
            ),
            (
                "src/a.rs",
                "use crate::A as Arg;\n\npub struct S<T>(pub T);\n\nimpl<T: Copy> S<T> {\n    pub fn f(&self, _: T, _: Arg) {}\n}\n",
            ),
            ("src/b.rs", ""),
        ],
        "base",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod a;\npub mod b;\npub use b::S;\npub struct A;\npub struct B;\n",
    )
    .expect("write");
    fs::write(repo.join("src/a.rs"), "").expect("write");
    fs::write(
        repo.join("src/b.rs"),
        "use crate::B as Arg;\n\npub struct S<T>(pub T);\n\nimpl<U> S<U> {\n    pub fn f(&self, _: U, _: Arg) {}\n}\n",
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 見出しの型の引数の改名とともに、`self: &Self` を通常の引数にすれば報告し、型の引数の末尾のカンマだけなら
/// 報告しない。対象の型に現れない引数が 7 個でも、その引数だけの境界 (`U: Copy`) を外しただけなら互換。
#[test]
fn renamed_members_keep_receivers_and_formatting() {
    let api = api_changes(
        "pub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f(self: &Self) {}\n}\n",
        "pub struct S<T>(pub T);\n\nimpl<U> S<U> {\n    pub fn f(_: &Self) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let api = api_changes(
        "pub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f<V>(&self) {}\n}\n",
        "pub struct S<T>(pub T);\n\nimpl<U> S<U> {\n    pub fn f<V,>(&self) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
    let head = "pub trait Tr {\n    type A;\n    type B;\n    type C;\n    type D;\n    type E;\n    type F;\n    type G;\n}\npub struct S<T>(pub T);\n\n";
    let tail = "\nwhere\n    T: Tr<A = U, B = V, C = W, D = X, E = Y, F = Z, G = Q>,\n{\n    pub fn f(&self) {}\n}\n";
    let api = api_changes(
        &format!("{head}impl<T, U: Copy, V: Copy, W, X, Y, Z, Q> S<T>{tail}"),
        &format!("{head}impl<T, U, V: Copy, W, X, Y, Z, Q> S<T>{tail}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod_compat", "{api:?}");
}

/// メンバーの signature に現れる、対象の型に現れない引数は、そこでの位置で組にする (impl の寿命の並べ替えは報告
/// せず、第 1 引数の寿命の境界を外しただけなら互換)。1 要素のタプルのカンマは残す。関連型の名前は型の引数の
/// 出現に数えない。ほかの引数と一緒の境界 (`V: AsRef<W>`) を外しただけなら、引数が 7 個でも互換。
#[test]
fn member_positions_decide_free_parameters() {
    let api = api_changes(
        "pub struct S;\n\nimpl<'a, 'b> S {\n    pub fn f(&self, _: &'a u8, _: &'b u8) {}\n}\n",
        "pub struct S;\n\nimpl<'b, 'a> S {\n    pub fn f(&self, _: &'a u8, _: &'b u8) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
    let api = api_changes(
        "pub struct S;\n\nimpl<'a: 'static, 'b, 'c, 'd> S {\n    pub fn f(&self, _: &'a u8, _: &'b u8, _: &'c u8, _: &'d u8) {}\n}\n",
        "pub struct S;\n\nimpl<'w, 'x, 'y, 'z> S {\n    pub fn f(&self, _: &'z u8, _: &'y u8, _: &'x u8, _: &'w u8) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "mod_compat", "{api:?}");
    let api = api_changes(
        "pub struct S<T>(pub T);\n\nimpl<T> S<T> {\n    pub fn f(&self, _: (u8,)) {}\n}\n",
        "pub struct S<T>(pub T);\n\nimpl<U> S<U> {\n    pub fn f(&self, _: (u8)) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let api = api_changes(
        "pub trait Tr {\n    type A;\n}\npub struct S<T>(pub T);\n\nimpl<T: Tr, U, A> S<(T, T::A, U, A)> {\n    pub fn f(&self) {}\n}\n",
        "pub trait Tr {\n    type A;\n}\npub struct S<T>(pub T);\n\nimpl<T: Tr, U, V> S<(T, T::A, U, V)> {\n    pub fn f(&self) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
    let head = "pub trait Tr {\n    type A;\n    type B;\n    type C;\n    type D;\n    type E;\n    type F;\n    type G;\n}\npub struct S<T>(pub T);\n\n";
    let tail = "\nwhere\n    T: Tr<A = U, B = V, C = W, D = X, E = Y, F = Z, G = Q>,\n{\n    pub fn f(&self) {}\n}\n";
    let api = api_changes(
        &format!("{head}impl<T, U, V: AsRef<W>, W, X, Y, Z, Q> S<T>{tail}"),
        &format!("{head}impl<T, U, V, W, X, Y, Z, Q> S<T>{tail}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod_compat", "{api:?}");
}

/// マクロの引数の中に型の引数が現れて名前の付け直しが旧新で違えば、対応を確かめられないので報告する。マクロの
/// 引数の中の末尾のカンマは変更として比べる。対象の型の型の引数の末尾のカンマだけなら報告しない。
#[test]
fn macro_inputs_and_self_type_commas_follow_rust_rules() {
    let head = "macro_rules! ty {\n    ($t:ty) => {\n        $t\n    };\n}\npub struct S;\n\n";
    let body = "{\n    pub fn f(&self, _: ty!(&'a u8), _: ty!(&'b u8)) {}\n}\n";
    let api = api_changes(
        &format!("{head}impl<'a: 'static, 'b> S {body}"),
        &format!("{head}impl<'a, 'b: 'static> S {body}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "macro_rules! arg {\n    (Vec<$t:ty,>) => {\n        ($t,)\n    };\n    (Vec<$t:ty>) => {\n        $t\n    };\n}\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<T> S<T> {{\n    pub fn f(&self, _: arg!(Vec<u8,>)) {{}}\n}}\n"),
        &format!("{head}impl<U> S<U> {{\n    pub fn f(&self, _: arg!(Vec<u8>)) {{}}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let api = api_changes(
        &source("impl<T> S<T>", METHOD),
        &source("impl<T> S<T,>", METHOD),
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
}

/// マクロの引数の中の型の引数は、その名前の番号が旧新で違えば対応を確かめられない (`r#T` も `T`)。マクロは引数に
/// 書かない型の引数も呼び出し側の名前で使えるので、ほかの引数の改名だけでも報告する。定数の値の中のマクロは値の
/// 変更として比べる。
#[test]
fn macro_inputs_compare_parameter_numbers() {
    let head = "macro_rules! ty {\n    ($t:ty) => {\n        $t\n    };\n}\npub struct S<T, U>(pub T, pub U);\n\n";
    let api = api_changes(
        &format!("{head}impl<T, U> S<T, U> {{\n    pub fn f(&self, _: ty!(r#T)) {{}}\n}}\n"),
        &format!("{head}impl<T, U> S<U, T> {{\n    pub fn f(&self, _: ty!(r#T)) {{}}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let api = api_changes(
        &format!("{head}impl<T, U> S<T, U> {{\n    pub fn f(&self, _: ty!(T)) {{}}\n}}\n"),
        &format!("{head}impl<T, V> S<T, V> {{\n    pub fn f(&self, _: ty!(T)) {{}}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "macro_rules! size {\n    ($t:ty) => {\n        core::mem::size_of::<$t>()\n    };\n}\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<T> S<T> {{\n    pub const N: usize = size!(T);\n}}\n"),
        &format!("{head}impl<U> S<U> {{\n    pub const N: usize = 0;\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.N"), "const_value", "{api:?}");
}

/// マクロの引数の中に現れる型の引数は名前で組にし、並べ替えの探索で付け替えない。宣言の raw の寿命 (`'r#a`) も
/// 同じ名前とする。関連定数の値の中の引数の対応が変われば、字面が同じでも値の変更。
#[test]
fn macro_parameters_pair_by_name() {
    let macro_head = "macro_rules! ty {\n    ($t:ty) => {\n        $t\n    };\n}\n";
    let head =
        format!("{macro_head}pub struct S<'x, 'y, 'z>(pub &'x u8, pub &'y u8, pub &'z u8);\n\n");
    let body = "{\n    pub fn f(&self, _: ty!(&'a u8), _: ty!(&'b u8)) {}\n}\n";
    let api = api_changes(
        &format!("{head}impl<'x, 'y, 'z, 'a: 'x + 'z, 'b: 'y> S<'x, 'y, 'z> {body}"),
        &format!("{head}impl<'x, 'y, 'z, 'a: 'y, 'b: 'z> S<'x, 'y, 'z> {body}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = format!("{macro_head}pub struct S;\n\n");
    let api = api_changes(
        &format!("{head}impl<'a, 'b: 'static> S {body}"),
        &format!("{head}impl<'a, 'b> S {body}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod_compat", "{api:?}");
    let raw_body = "{\n    pub fn f(&self, _: ty!(&'r#a u8), _: ty!(&'r#b u8)) {}\n}\n";
    let api = api_changes(
        &format!("{head}impl<'r#a: 'static, 'r#b> S {raw_body}"),
        &format!("{head}impl<'r#a, 'r#b: 'static> S {raw_body}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "macro_rules! size {\n    ($t:ty) => {\n        core::mem::size_of::<$t>()\n    };\n}\npub struct S<T, U>(pub T, pub U);\n\n";
    let api = api_changes(
        &format!("{head}impl<T, U> S<T, U> {{\n    pub const N: usize = size!(T);\n}}\n"),
        &format!("{head}impl<T, U> S<U, T> {{\n    pub const N: usize = size!(T);\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.N"), "const_value", "{api:?}");
}

/// マクロは引数に書かない型の引数も呼び出し側の名前で使えるので、名前の付け直しが旧新で違えば報告する。寿命と
/// 型の引数は名前空間で区別する。通常の定数式も、型の引数の対応が変われば (字面が同じでも) 値の変更。移動でも
/// 値の対応が変われば報告する。
#[test]
fn macros_use_caller_names() {
    let head = "macro_rules! ty {\n    ($t:ty) => {\n        $t\n    };\n}\npub struct S<T, U>(pub T, pub U);\n\n";
    let api = api_changes(
        &format!("{head}impl<'T, T, U> S<T, U> {{\n    pub fn f(&'T self, _: ty!(T)) {{}}\n}}\n"),
        &format!("{head}impl<'T, T, U> S<U, T> {{\n    pub fn f(&'T self, _: ty!(T)) {{}}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "macro_rules! ty {\n    () => {\n        T\n    };\n}\npub struct S<T, U>(pub T, pub U);\n\n";
    let api = api_changes(
        &format!("{head}impl<T: Copy, U> S<T, U> {{\n    pub fn f(&self, _: ty!()) {{}}\n}}\n"),
        &format!("{head}impl<T, U> S<U, T> {{\n    pub fn f(&self, _: ty!()) {{}}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "pub struct S<T, U>(pub T, pub U);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T, U> S<T, U> {{\n    pub const N: usize = core::mem::size_of::<T>();\n}}\n"
        ),
        &format!(
            "{head}impl<T, U> S<U, T> {{\n    pub const N: usize = core::mem::size_of::<T>();\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.N"), "const_value", "{api:?}");
    let head = "macro_rules! size {\n    ($t:ty) => {\n        core::mem::size_of::<$t>()\n    };\n}\npub mod methods;\npub struct S<T, U>(pub T, pub U);\n";
    let api = moved_changes(
        &format!("{head}\nimpl<T: Copy, U> S<T, U> {{\n    pub const N: usize = size!(T);\n}}\n"),
        head,
        "use crate::S;\n\nimpl<T, U> S<U, T> {\n    pub const N: usize = size!(T);\n}\n",
    );
    assert_eq!(bucket(&api, "S.N"), "mod", "{api:?}");
}

/// 並べ替えの探索で採った引数の対応で、マクロの呼び出しを含むメンバーと定数の値も比べる。使われない寿命の引数は
/// 比べない。
#[test]
fn searched_pairings_recheck_members() {
    let head = "macro_rules! arg {\n    () => {\n        U\n    };\n}\npub trait A {\n    type X;\n}\npub trait B {\n    type X;\n}\npub trait C {\n    type X;\n}\npub struct S<T>(pub T);\n\n";
    let impls = |bounds: &str, member: &str| {
        format!("{head}impl<T, U, V> S<T>\nwhere\n    T: {bounds},\n{{\n    {member}\n}}\n")
    };
    let api = api_changes(
        &impls(
            "A<X = V> + B<X = U> + C<X = V>",
            "pub fn f(&self, _: arg!()) {}",
        ),
        &impls("B<X = V> + C<X = U>", "pub fn f(&self, _: arg!()) {}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let member = "pub const N: usize = core::mem::size_of::<U>();";
    let api = api_changes(
        &impls("A<X = V> + B<X = U> + C<X = V>", member),
        &impls("B<X = V> + C<X = U>", member),
    );
    assert_eq!(bucket(&api, "S.N"), "mod", "{api:?}");
    let api = api_changes(
        "pub struct S;\n\nimpl<'a> S {\n    pub fn f(&self) {}\n}\n",
        "pub struct S;\n\nimpl S {\n    pub fn f(&self) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
}

/// 戻り値が `impl Trait` の関数は本体も契約の部分 (隠れた型の auto trait が漏れる)。使われない寿命の引数は比べず、
/// 並べ替えの探索の邪魔にもしない。
#[test]
fn hidden_returns_and_unused_lifetimes() {
    let head = "pub struct S<T, U>(pub T, pub U);\n\n";
    let body =
        "{\n    pub fn f(&self) -> impl Copy {\n        core::marker::PhantomData::<T>\n    }\n}\n";
    let api = api_changes(
        &format!("{head}impl<T: Copy, U> S<T, U> {body}"),
        &format!("{head}impl<T, U> S<U, T> {body}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let api = api_changes(
        "pub struct S;\n\nimpl<'a, 'b: 'static> S {\n    pub fn f(&self) {}\n}\n",
        "pub struct S;\n\nimpl<'b: 'static> S {\n    pub fn f(&self) {}\n}\n",
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
    let head = "pub trait A {\n    type X;\n}\npub trait B {\n    type X;\n}\npub trait C {\n    type X;\n}\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<'a, T, U, V> S<T>\nwhere\n    T: A<X = V> + B<X = U> + C<X = V>,\n{{\n{METHOD}\n}}\n"
        ),
        &format!(
            "{head}impl<T, U, V> S<T>\nwhere\n    T: B<X = U> + C<X = V>,\n{{\n{METHOD}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod_compat", "{api:?}");
}

/// 戻り値が隠れた型の関数は、見出しを変えたら引数のパターンと本体も比べる (付け直しが同じでも)。`async` は
/// コメントが隣り合っても見る。項目の位置のマクロの呼び出しが作る型は、const の引数として置き換えない。
#[test]
fn hidden_returns_compare_patterns_and_bodies() {
    let head = "use std::{marker::PhantomData, rc::Rc};\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T: Copy> S<T> {{\n    pub fn f((x, _): (u8, PhantomData<Rc<()>>)) -> impl Copy {{\n        x\n    }}\n}}\n"
        ),
        &format!(
            "{head}impl<U> S<U> {{\n    pub fn f((_, x): (u8, PhantomData<Rc<()>>)) -> impl Copy {{\n        x\n    }}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "pub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T: Copy> S<T> {{\n    pub fn f() -> impl Copy {{\n        0u8\n    }}\n}}\n"
        ),
        &format!(
            "{head}impl<T> S<T> {{\n    pub fn f() -> impl Copy {{\n        core::marker::PhantomData::<std::rc::Rc<()>>\n    }}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "pub struct S<T, U>(pub T, pub U);\n\n";
    let body = "{\n    pub async/* comment */unsafe fn f() {\n        let values = Vec::<T>::new();\n        core::future::pending::<()>().await;\n        drop(values);\n    }\n}\n";
    let api = api_changes(
        &format!("{head}impl<T: Copy, U> S<T, U> {body}"),
        &format!("{head}impl<T, U> S<U, T> {body}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "macro_rules! types {\n    () => {\n        pub struct A {}\n        pub struct B {}\n    };\n}\ntypes!();\n\npub struct S<T, const N: usize>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<const A: usize> S<A, {{ A }}> {{\n{METHOD}\n}}\n"),
        &format!("{head}impl<const B: usize> S<B, {{ B }}> {{\n{METHOD}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 戻り値の型にマクロの呼び出しがあれば隠れた型の関数と同じに本体も比べる。標準でない derive が作る型は const の
/// 引数として置き換えない。隠れた型の関数の引数の省略できる最後のカンマは変更にしない (改名と同時でも)。
#[test]
fn macro_returns_derives_and_parameter_commas() {
    let head = "macro_rules! ret {\n    () => {\n        impl Copy\n    };\n}\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T: Copy> S<T> {{\n    pub fn f() -> ret!() {{\n        0u8\n    }}\n}}\n"
        ),
        &format!(
            "{head}impl<T> S<T> {{\n    pub fn f() -> ret!() {{\n        core::marker::PhantomData::<std::rc::Rc<()>>\n    }}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "#[derive(derive_builder::Builder)]\npub struct A {\n    pub x: u8,\n}\n\n#[derive(derive_builder::Builder)]\npub struct B {\n    pub x: u8,\n}\n\npub struct S<T, const N: usize>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<const ABuilder: usize> S<ABuilder, {{ ABuilder }}> {{\n{METHOD}\n}}\n"
        ),
        &format!(
            "{head}impl<const BBuilder: usize> S<BBuilder, {{ BBuilder }}> {{\n{METHOD}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "pub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T> S<T> {{\n    pub fn f(x: u8) -> impl Copy {{\n        x\n    }}\n}}\n"
        ),
        &format!(
            "{head}impl<U> S<U> {{\n    pub fn f(x: u8,) -> impl Copy {{\n        x\n    }}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
}

/// 隠れた型の関数は、見出しの境界を外すだけでも本体のメソッドの解決が変わり得るので報告する。`cfg_attr` の中の
/// derive が作る型も const の引数として置き換えない。`for<'a>` の束縛の改名は報告しない。
#[test]
fn header_changes_with_hidden_returns_and_binders() {
    let head = "use std::{marker::PhantomData, rc::Rc};\n\npub trait Primary {\n    fn pick(self);\n}\nimpl Primary for u8 {\n    fn pick(self) {}\n}\n\ntrait Fallback {\n    fn pick(self) -> PhantomData<Rc<()>>;\n}\nimpl<T> Fallback for T {\n    fn pick(self) -> PhantomData<Rc<()>> {\n        PhantomData\n    }\n}\n\npub struct S<T>(pub T);\n\n";
    let body = "{\n    pub fn f(x: T) -> impl Copy {\n        x.pick()\n    }\n}\n";
    let api = api_changes(
        &format!("{head}impl<T: Primary> S<T> {body}"),
        &format!("{head}impl<T> S<T> {body}"),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head = "#[cfg_attr(all(), derive(derive_builder::Builder))]\npub struct A {\n    pub x: u8,\n}\n\n#[cfg_attr(all(), derive(derive_builder::Builder))]\npub struct B {\n    pub x: u8,\n}\n\npub struct S<T, const N: usize>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<const ABuilder: usize> S<ABuilder, {{ ABuilder }}> {{\n{METHOD}\n}}\n"
        ),
        &format!(
            "{head}impl<const BBuilder: usize> S<BBuilder, {{ BBuilder }}> {{\n{METHOD}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
    let head =
        "pub trait Bound<'a> {}\nimpl<'a> Bound<'a> for &'a u8 {}\n\npub struct S<T>(pub T);\n\n";
    let api = api_changes(
        &format!("{head}impl<T> S<T>\nwhere\n    for<'a> &'a T: Bound<'a>,\n{{\n{METHOD}\n}}\n"),
        &format!("{head}impl<T> S<T>\nwhere\n    for<'b> &'b T: Bound<'b>,\n{{\n{METHOD}\n}}\n"),
    );
    assert_eq!(bucket(&api, "S.f"), "none", "{api:?}");
}

/// 型の位置に書いた const の引数は名前のまま比べるので、名前を変えずに位置を入れ替え、同じ字面 (`Bound<A>`) が別の
/// 引数を指すようになれば報告する。
#[test]
fn typed_const_parameters_keep_their_positions() {
    let head = "pub trait Bound<const N: usize> {}\nimpl Bound<1> for u8 {}\n\npub struct S<T, const N: usize, const M: usize>(pub T);\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<T: Bound<A>, const A: usize, const B: usize> S<T, {{ A }}, {{ B }}> {{\n{METHOD}\n}}\n"
        ),
        &format!(
            "{head}impl<T: Bound<A>, const A: usize, const B: usize> S<T, {{ B }}, {{ A }}> {{\n{METHOD}\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.f"), "mod", "{api:?}");
}

/// 型の位置に書いた const の引数の番号が定数の値の中だけで変われば、値の変更 (契約の変更にしない)。
#[test]
fn typed_const_parameters_in_values_are_value_changes() {
    let head = "pub struct S<const N: usize, const M: usize>;\nconst fn value<const K: usize>() -> usize {\n    K\n}\n\n";
    let api = api_changes(
        &format!(
            "{head}impl<const A: usize, const B: usize> S<{{ A }}, {{ B }}> {{\n    pub const C: usize = value::<A>();\n}}\n"
        ),
        &format!(
            "{head}impl<const A: usize, const B: usize> S<{{ B }}, {{ A }}> {{\n    pub const C: usize = value::<A>();\n}}\n"
        ),
    );
    assert_eq!(bucket(&api, "S.C"), "const_value", "{api:?}");
}
