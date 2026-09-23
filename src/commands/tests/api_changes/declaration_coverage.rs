//! 宣言の取りこぼし (分割代入・`var`・generator・abstract class・record・trait メソッド) と、
//! 関数値バインディングの本体変更を扱う API 差分テスト。

use crate::commands::tests::common::*;
use crate::commands::*;
use std::fs;

/// 作業ツリーの変更 (HEAD 比) に対する API 差分を返す。
fn api_after_edit(repo: &std::path::Path) -> crate::models::review::ApiChanges {
    let (diff, _) = resolve_git_diff_parts(repo);
    let diff_files = crate::engine::diff::parse_unified_diff(&diff);
    detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files)
}

fn removed_names(api: &crate::models::review::ApiChanges) -> Vec<&str> {
    api.removed.iter().map(|s| s.name.as_str()).collect()
}

fn modified_names(api: &crate::models::review::ApiChanges) -> Vec<&str> {
    api.modified.iter().map(|s| s.name.as_str()).collect()
}

/// 分割代入と `var` の export を削除すると api.rm に出る。生き残った兄弟の束縛は
/// 「その名前へ至る経路 + 初期化子」で比較するので api.mod に出ない。
///
/// 旧実装はこれらをシンボルとして抽出しておらず、import されたままの名前を消しても
/// `review --hook` が exit 0 だった (Auth.js v5 の `export const { auth } = NextAuth()`)。
#[test]
fn destructured_and_var_exports_are_part_of_the_api_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/auth.ts",
                "export const { handlers, auth, signIn, signOut } = NextAuth({});\n\
                 export const [first, second] = pair();\n\
                 export const [left, right] = pair2();\n\
                 export var legacy = 1;\n",
            ),
            (
                "src/use.ts",
                "import { handlers, auth, signIn, signOut, first, second, left, right, legacy } from \"./auth\";\n\
                 export const all = [handlers, auth, signIn, signOut, first, second, left, right, legacy];\n",
            ),
        ],
        "initial",
    );
    // signOut / first / legacy を削除する。second は穴で添字 1 を保つ (契約不変)。
    // right は `[left, right]` → `[right]` で添字 1 → 0 に変わる (契約変更の対照)。
    fs::write(
        repo.join("src/auth.ts"),
        "export const { handlers, auth, signIn } = NextAuth({});\n\
         export const [, second] = pair();\n\
         export const [right] = pair2();\n",
    )
    .expect("write");
    let api = api_after_edit(repo);
    let removed = removed_names(&api);
    for name in ["signOut", "first", "legacy", "left"] {
        assert!(removed.contains(&name), "{name} should be api.rm: {api:?}");
    }
    let modified = modified_names(&api);
    for name in ["handlers", "auth", "signIn", "second"] {
        assert!(
            !modified.contains(&name),
            "{name} keeps its contract and must not be api.mod: {modified:?}"
        );
    }
    assert!(
        modified.contains(&"right"),
        "array position change is a contract change: {modified:?}"
    );
}

/// rest や default を含む分割代入は兄弟の集合・評価で中身が変わりうるため、
/// 束縛ごとの正規化をせず宣言全体で比較する (保守側)。
#[test]
fn destructuring_with_rest_or_default_compares_the_whole_declaration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/cfg.ts",
                "export const { a, b, ...rest } = cfg();\n\
                 export const { x = 1, y } = opts();\n",
            ),
            (
                "src/use.ts",
                "import { a, rest, x, y } from \"./cfg\";\nexport const all = [a, rest, x, y];\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/cfg.ts"),
        "export const { a, ...rest } = cfg();\n\
         export const { x = 2, y } = opts();\n",
    )
    .expect("write");
    let api = api_after_edit(repo);
    let modified = modified_names(&api);
    // b を消すと rest の中身が変わる。default の変更は x だけでなく兄弟の y にも倒す。
    for name in ["rest", "x", "y"] {
        assert!(modified.contains(&name), "{name}: {modified:?}");
    }
}

/// 値そのものが関数のバインディングは本体を signature から省く (関数宣言と揃える)。
///
/// 旧実装は宣言全体を signature にしていたため、`export const Button = () => {..}` の
/// JSX を直しただけで blocking な api.mod になっていた。React の HOC (`forwardRef`)
/// とオブジェクトリテラルのメソッドも同様。任意の呼び出しのコールバック
/// (`create((set) => ({..}))`) はストアの形そのものなので本体を省かない (対照)。
#[test]
fn function_valued_binding_body_change_is_not_api_mod() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let before = "export const Arrow = () => {\n  return <button className=\"a\">x</button>;\n};\n\
                  export const Fwd = React.forwardRef<HTMLButtonElement, Props>((props, ref) => {\n  return <button ref={ref} className=\"a\" />;\n});\n\
                  export const api = {\n  get() {\n    return 1;\n  },\n  post: () => {\n    return 1;\n  },\n};\n\
                  export const Param = (a: number) => {\n  return a;\n};\n\
                  export const useStore = create((set) => ({\n  count: 0,\n  inc: () => set((s) => ({ count: s.count + 1 })),\n}));\n";
    git_commit_files(
        repo,
        &[
            ("src/lib.tsx", before),
            (
                "src/use.tsx",
                "import { Arrow, Fwd, api, Param, useStore } from \"./lib\";\n\
                 export const all = [Arrow, Fwd, api.get, api.post, Param, useStore];\n",
            ),
        ],
        "initial",
    );
    let after = before
        .replace("<button className=\"a\">x", "<button className=\"a b\">x")
        .replace("ref={ref} className=\"a\"", "ref={ref} className=\"a b\"")
        .replace("get() {\n    return 1;", "get() {\n    return 2;")
        .replace(
            "post: () => {\n    return 1;",
            "post: () => {\n    return 2;",
        )
        .replace("(a: number) =>", "(a: number, b: number) =>")
        .replace("  inc: () => set((s) => ({ count: s.count + 1 })),\n", "");
    fs::write(repo.join("src/lib.tsx"), after).expect("write");
    let api = api_after_edit(repo);
    let modified = modified_names(&api);
    for name in ["Arrow", "Fwd", "api"] {
        assert!(
            !modified.contains(&name),
            "body-only change of {name} must not be api.mod: {modified:?}"
        );
    }
    assert!(
        modified.contains(&"Param"),
        "parameter change stays api.mod: {modified:?}"
    );
    assert!(
        modified.contains(&"useStore"),
        "store shape change stays api.mod: {modified:?}"
    );
}

/// generator 関数と abstract class の削除は api.rm に出る (旧実装はシンボル化していなかった)。
#[test]
fn generator_and_abstract_class_removals_are_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib.ts",
                "export function* ids() { yield 1; }\n\
                 export abstract class Shape { describe() { return 1; } }\n\
                 export function plain() { return 1; }\n",
            ),
            (
                "src/use.ts",
                "import { ids, Shape, plain } from \"./lib\";\n\
                 export class Sq extends Shape {}\n\
                 export const all = [ids(), plain()];\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib.ts"),
        "export function plain() { return 1; }\n",
    )
    .expect("write");
    let api = api_after_edit(repo);
    let removed = removed_names(&api);
    for name in ["ids", "Shape"] {
        assert!(removed.contains(&name), "{name}: {removed:?}");
    }
}

/// Java の record の削除は api.rm に出る。
#[test]
fn java_record_removal_is_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/Point.java",
                "public record Point(int x, int y) {\n    public int sum() { return x + y; }\n}\n",
            ),
            (
                "src/Use.java",
                "public class Use {\n    int f() { return new Point(1, 2).sum(); }\n}\n",
            ),
        ],
        "initial",
    );
    fs::remove_file(repo.join("src/Point.java")).expect("remove");
    let api = api_after_edit(repo);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(removed.contains(&"Point"), "{api:?}");
}

/// `pub trait` のメソッド (必須 / default) のシグネチャ変更は api.mod に出る。
/// `pub(crate) trait` はクレート外から見えないので出ない (対照)。
///
/// 旧実装は trait 本体のメソッドを「可視性修飾子なし = 非公開」と扱い、必須メソッドは
/// シンボル化すらしていなかったため、公開 trait の契約変更が一切報告されなかった。
#[test]
fn rust_pub_trait_method_signature_change_is_api_mod() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"lib1\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
            ),
            (
                "src/lib.rs",
                "pub trait Shape {\n    fn area(&self) -> f64;\n    fn scale(&self, k: f64) -> f64 { k }\n}\n\
                 pub(crate) trait Internal {\n    fn hidden(&self) -> u32;\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub trait Shape {\n    fn area(&self, unit: u8) -> f64;\n    fn scale(&self, k: f64, j: f64) -> f64 { k + j }\n}\n\
         pub(crate) trait Internal {\n    fn hidden(&self, x: u8) -> u32;\n}\n",
    )
    .expect("write");
    let api = api_after_edit(repo);
    let modified = modified_names(&api);
    for name in ["Shape.area", "Shape.scale"] {
        assert!(modified.contains(&name), "{name}: {modified:?}");
    }
    assert!(
        !modified.iter().any(|n| n.ends_with("hidden")),
        "crate-internal trait is not public API: {modified:?}"
    );
}
