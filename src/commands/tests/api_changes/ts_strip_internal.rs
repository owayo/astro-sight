//! TypeScript の `stripInternal` で宣言ファイルから消える `@internal` の宣言 (Issue #274)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

const STRIP: &str = "{ \"compilerOptions\": { \"declaration\": true, \"stripInternal\": true } }\n";
const NO_STRIP: &str = "{ \"compilerOptions\": { \"declaration\": true } }\n";

/// `files` をコミットし、`changes` に書き換えたときの `name` の区分 (`mod` / `rm` / `rm_dead` /
/// `none`)。
fn bucket(files: &[(&str, &str)], changes: &[(&str, &str)], name: &str) -> &'static str {
    bucket_after(files, changes, &[], name)
}

/// [`bucket`] で、`removed` のファイルも消す (空になったディレクトリも消す)。
fn bucket_after(
    files: &[(&str, &str)],
    changes: &[(&str, &str)],
    removed: &[&str],
    name: &str,
) -> &'static str {
    classify(&changed_api(files, changes, removed), name)
}

/// API の差分の `name` の区分。
fn classify(api: &crate::models::review::ApiChanges, name: &str) -> &'static str {
    if api.modified.iter().any(|change| change.name == name) {
        "mod"
    } else if api.removed.iter().any(|symbol| symbol.name == name) {
        "rm"
    } else if api.removed_dead.iter().any(|symbol| symbol.name == name) {
        "rm_dead"
    } else {
        "none"
    }
}

/// `files` をコミットし、`changes` に書き換えて `removed` を消したときの API の差分。
fn changed_api(
    files: &[(&str, &str)],
    changes: &[(&str, &str)],
    removed: &[&str],
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, files, "base");
    for (path, content) in changes {
        fs::write(repo.join(path), content).expect("write");
    }
    for path in removed {
        let path = repo.join(path);
        fs::remove_file(&path).expect("remove");
        let _ = fs::remove_dir(path.parent().expect("parent"));
    }
    detect_api_changes_from_worktree(repo)
}

const OPTIONS: &str = "export interface Options {\n  name: string;\n  retries: number;\n}\n\nexport function connect(options: Options): string {\n  return options.name;\n}\n";
const OPTIONS_INTERNAL_MEMBER: &str = "export interface Options {\n  name: string;\n  /** @internal */\n  retries: number;\n}\n\nexport function connect(options: Options): string {\n  return options.name;\n}\n";
const OPTIONS_INTERNAL_EXPORT: &str = "export interface Options {\n  name: string;\n  retries: number;\n}\n\n/** @internal */\nexport function connect(options: Options): string {\n  return options.name;\n}\n";

/// `stripInternal` の設定で `@internal` を足すと、宣言ファイルから消えるので削除と同じ区分になる
/// (Issue #274 の再現)。
#[test]
fn ts_internal_tag_with_strip_internal_removes_from_declarations() {
    // interface のメンバー: 型のメンバーの削除 (blocking な api.mod)
    assert_eq!(
        bucket(
            &[("tsconfig.json", STRIP), ("src/lib.ts", OPTIONS)],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "mod"
    );
    // 参照の無い export: export の削除と同じ api.rm_dead
    assert_eq!(
        bucket(
            &[("tsconfig.json", STRIP), ("src/lib.ts", OPTIONS)],
            &[("src/lib.ts", OPTIONS_INTERNAL_EXPORT)],
            "connect"
        ),
        "rm_dead"
    );
    // リポジトリの中で使う export: blocking な api.rm
    let user = "import { connect } from \"./lib\";\n\nexport const label = connect({ name: \"a\", retries: 1 });\n";
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", STRIP),
                ("src/lib.ts", OPTIONS),
                ("src/user.ts", user)
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_EXPORT)],
            "connect"
        ),
        "rm"
    );
    // `extends` の先で有効にした設定と、行コメントの `@internal` も同じ
    assert_eq!(
        bucket(
            &[
                ("tsconfig.base.json", STRIP),
                (
                    "tsconfig.json",
                    "{ \"extends\": \"./tsconfig.base.json\" }\n"
                ),
                ("src/lib.ts", OPTIONS)
            ],
            &[(
                "src/lib.ts",
                "export interface Options {\n  name: string;\n  // @internal\n  retries: number;\n}\n\nexport function connect(options: Options): string {\n  return options.name;\n}\n"
            )],
            "Options"
        ),
        "mod"
    );
    // クラスのメソッド
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", STRIP),
                (
                    "src/client.ts",
                    "export class Client {\n  open(): void {}\n  close(): void {}\n}\n"
                )
            ],
            &[(
                "src/client.ts",
                "export class Client {\n  open(): void {}\n  /** @internal */\n  close(): void {}\n}\n"
            )],
            "Client.close"
        ),
        "rm_dead"
    );
}

/// 対照: `stripInternal` が無い設定と、出力しない手書きの宣言ファイルでは `@internal` は宣言
/// ファイルを変えない。すでに `@internal` の宣言の削除は、宣言ファイルを使う利用者から見て無い。
#[test]
fn ts_internal_tag_without_stripping_is_not_reported() {
    assert_eq!(
        bucket(
            &[("tsconfig.json", NO_STRIP), ("src/lib.ts", OPTIONS)],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "none"
    );
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", STRIP),
                (
                    "types/lib.d.ts",
                    "export interface Options {\n  name: string;\n  retries: number;\n}\n"
                )
            ],
            &[(
                "types/lib.d.ts",
                "export interface Options {\n  name: string;\n  /** @internal */\n  retries: number;\n}\n"
            )],
            "Options"
        ),
        "none"
    );
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", STRIP),
                ("src/lib.ts", OPTIONS_INTERNAL_EXPORT)
            ],
            &[(
                "src/lib.ts",
                "export interface Options {\n  name: string;\n  retries: number;\n}\n"
            )],
            "connect"
        ),
        "none"
    );
}

/// 入れ子の型のメンバー、`export { x }` 文、オーバーロードの宣言の `@internal` も宣言ファイルから
/// 消える。実装の宣言の `@internal` と、前の宣言の行末のコメントは消さない。
#[test]
fn ts_internal_tag_follows_declaration_emit() {
    let files = |lib: &'static str| [("tsconfig.json", STRIP), ("src/lib.ts", lib)];
    // 入れ子の型のメンバー
    assert_eq!(
        bucket(
            &files(
                "export interface Options {\n  transport: {\n    host: string;\n    retries: number;\n  };\n}\n"
            ),
            &[(
                "src/lib.ts",
                "export interface Options {\n  transport: {\n    host: string;\n    /** @internal */\n    retries: number;\n  };\n}\n"
            )],
            "Options"
        ),
        "mod"
    );
    // `export { x }` 文
    assert_eq!(
        bucket(
            &files("const x = 1;\nexport { x };\n"),
            &[(
                "src/lib.ts",
                "const x = 1;\n/** @internal */\nexport { x };\n"
            )],
            "x"
        ),
        "rm_dead"
    );
    // オーバーロードが全て `@internal`
    let overloads = "export function f(x: string): string;\nexport function f(x: number): number;\nexport function f(x: string | number): string | number {\n  return x;\n}\n";
    assert_eq!(
        bucket(
            &files(overloads),
            &[(
                "src/lib.ts",
                "/** @internal */\nexport function f(x: string): string;\n/** @internal */\nexport function f(x: number): number;\nexport function f(x: string | number): string | number {\n  return x;\n}\n"
            )],
            "f"
        ),
        "rm_dead"
    );
    // 実装の宣言の `@internal` はオーバーロードを消さない
    assert_eq!(
        bucket(
            &files(overloads),
            &[(
                "src/lib.ts",
                "export function f(x: string): string;\nexport function f(x: number): number;\n/** @internal */\nexport function f(x: string | number): string | number {\n  return x;\n}\n"
            )],
            "f"
        ),
        "none"
    );
    // 前の宣言の行末のコメントは、次の宣言の leading comment ではない
    assert_eq!(
        bucket(
            &files(OPTIONS),
            &[(
                "src/lib.ts",
                "export interface Options {\n  name: string; /** @internal */\n  retries: number;\n}\n\nexport function connect(options: Options): string {\n  return options.name;\n}\n"
            )],
            "Options"
        ),
        "none"
    );
}

/// `stripInternal` は、ファイルに適用される設定の実効値で決める (子の設定の上書き、別の
/// プロジェクトの設定は効かない)。
#[test]
fn ts_internal_tag_follows_the_effective_project_setting() {
    // 子の設定が `false` で上書きする
    assert_eq!(
        bucket(
            &[
                ("config/base.json", STRIP),
                (
                    "tsconfig.json",
                    "{ \"extends\": \"./config/base.json\", \"compilerOptions\": { \"stripInternal\": false } }\n"
                ),
                ("src/lib.ts", OPTIONS)
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "none"
    );
    // 別のパッケージの設定は効かない
    assert_eq!(
        bucket(
            &[
                ("packages/a/tsconfig.json", STRIP),
                ("packages/b/tsconfig.json", NO_STRIP),
                ("packages/b/src/lib.ts", OPTIONS)
            ],
            &[("packages/b/src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "none"
    );
    // 宣言を出す別名の設定 (tsconfig.build.json) は効く
    assert_eq!(
        bucket(
            &[
                ("packages/a/tsconfig.json", NO_STRIP),
                ("packages/a/tsconfig.build.json", STRIP),
                ("packages/a/src/lib.ts", OPTIONS)
            ],
            &[("packages/a/src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "mod"
    );
}

/// 互換の判定も、宣言ファイルから消えるメンバーを除いた型で比べる。必須のメンバーを `@internal`
/// にして optional のメンバーを足す変更は、optional の追加だけの互換な変更ではない。
#[test]
fn ts_internal_tag_is_not_hidden_by_compatible_member_additions() {
    let added = "export interface Options {\n  name: string;\n  /** @internal */\n  retries: number;\n  debug?: boolean;\n}\n\nexport function connect(options: Options): string {\n  return options.name;\n}\n";
    let api = changed_api(
        &[("tsconfig.json", STRIP), ("src/lib.ts", OPTIONS)],
        &[("src/lib.ts", added)],
        &[],
    );
    assert!(
        api.modified.iter().any(|change| change.name == "Options"),
        "{api:?}"
    );
    assert!(
        !api.compatible_modified
            .iter()
            .any(|change| change.name == "Options"),
        "{api:?}"
    );
    // 消えるメンバーの外で optional のメンバーを足すだけなら、従来どおり互換
    let api = changed_api(
        &[
            ("tsconfig.json", STRIP),
            ("src/lib.ts", OPTIONS_INTERNAL_MEMBER),
        ],
        &[("src/lib.ts", added)],
        &[],
    );
    assert!(
        !api.modified.iter().any(|change| change.name == "Options"),
        "{api:?}"
    );
    assert!(
        api.compatible_modified
            .iter()
            .any(|change| change.name == "Options"),
        "{api:?}"
    );
}

/// 関数の引数の object 型の `@internal` のメンバーも宣言ファイルから消えるので、その型を変えても
/// 関数の API は変わらない。メンバーと区切りの間のコメント、前の宣言の行から続く複数行のコメントも
/// TypeScript と同じに扱う。
#[test]
fn ts_internal_members_follow_the_declaration_emitter() {
    let files = |lib: &'static str| [("tsconfig.json", STRIP), ("src/lib.ts", lib)];
    assert_eq!(
        bucket(
            &files(
                "export function connect(options: {\n  host: string;\n  /** @internal */\n  retries: number;\n}): void {}\n"
            ),
            &[(
                "src/lib.ts",
                "export function connect(options: {\n  host: string;\n  /** @internal */\n  retries: string;\n}): void {}\n"
            )],
            "connect"
        ),
        "none"
    );
    // 消えるメンバーと区切りの間にコメントがある
    assert_eq!(
        bucket(
            &files(
                "export interface Options {\n  transport: {\n    /** @internal */\n    retries: number /* unit */;\n    host: string;\n  };\n}\n"
            ),
            &[(
                "src/lib.ts",
                "export interface Options {\n  transport: {\n    host: string;\n  };\n}\n"
            )],
            "Options"
        ),
        "none"
    );
    // 前の宣言の行から続く複数行のコメントの後ろは leading comment ではない
    assert_eq!(
        bucket(
            &files("const marker = 1; /* note\n */\nexport function f(): void {}\n"),
            &[(
                "src/lib.ts",
                "const marker = 1; /* note\n */ /** @internal */\nexport function f(): void {}\n"
            )],
            "f"
        ),
        "none"
    );
}

/// オーバーロードのある関数も、`@internal` の `export { f }` 文だけが公開するなら宣言ファイルから
/// 消える。
#[test]
fn ts_internal_export_clause_hides_overloaded_functions() {
    let overloads = "function f(x: string): string;\nfunction f(x: unknown): string {\n  return String(x);\n}\n";
    let before = format!("{overloads}export {{ f }};\n");
    let after = format!("{overloads}/** @internal */\nexport {{ f }};\n");
    assert_eq!(
        bucket(
            &[("tsconfig.json", STRIP), ("src/lib.ts", &before)],
            &[("src/lib.ts", &after)],
            "f"
        ),
        "rm_dead"
    );
}

/// ファイルを含まない設定は判定を止めない。入力の無い設定は数えず、ファイルを含まなくても
/// 入力のある出力の設定 (import で取り込む) は `stripInternal` を有効にする。
#[test]
fn ts_strip_internal_uses_configs_that_compile_the_file() {
    // `files: []` の土台の設定は、同じディレクトリの設定の `false` を覆さない
    assert_eq!(
        bucket(
            &[
                (
                    "tsconfig.base.json",
                    "{ \"compilerOptions\": { \"stripInternal\": true }, \"files\": [] }\n"
                ),
                (
                    "tsconfig.json",
                    "{ \"extends\": \"./tsconfig.base.json\", \"compilerOptions\": { \"declaration\": true, \"stripInternal\": false }, \"files\": [\"src/lib.ts\"] }\n"
                ),
                ("src/lib.ts", OPTIONS)
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "none"
    );
    // 近くの別の設定がファイルを含まなければ、祖先の出力の設定で決める
    assert_eq!(
        bucket(
            &[
                (
                    "tsconfig.json",
                    "{ \"compilerOptions\": { \"declaration\": true, \"stripInternal\": true }, \"files\": [\"src/lib.ts\"] }\n"
                ),
                (
                    "src/tsconfig.test.json",
                    "{ \"compilerOptions\": { \"noEmit\": true }, \"files\": [\"test.ts\"] }\n"
                ),
                ("src/test.ts", "export const ok = true;\n"),
                ("src/lib.ts", OPTIONS)
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "mod"
    );
    // 入口だけを `files` に書いた出力の設定が import で取り込むファイルにも、その設定の
    // `stripInternal` を当てる (Issue #288)
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", NO_STRIP),
                (
                    "tsconfig.build.json",
                    "{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": { \"stripInternal\": true }, \"files\": [\"src/index.ts\"] }\n"
                ),
                ("src/index.ts", "export * from \"./lib\";\n"),
                ("src/lib.ts", OPTIONS)
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "mod"
    );
    // `exclude` で外したファイルは、近い設定でも判定を止めない (祖先の出力の設定で決める)
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", STRIP),
                (
                    "src/tsconfig.json",
                    "{ \"compilerOptions\": { \"noEmit\": true }, \"exclude\": [\"legacy\"] }\n"
                ),
                ("src/main.ts", "export const main = 1;\n"),
                ("src/legacy/lib.ts", OPTIONS)
            ],
            &[("src/legacy/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "mod"
    );
    // 対照: 近い設定がファイルを含めば、その設定で決める
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", STRIP),
                (
                    "src/tsconfig.json",
                    "{ \"compilerOptions\": { \"noEmit\": true } }\n"
                ),
                ("src/legacy/lib.ts", OPTIONS)
            ],
            &[("src/legacy/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "none"
    );
}

/// `extends` の配列の後ろの設定の `null` は、前の設定の `true` を打ち消す。削除したファイルの
/// ディレクトリが無くなっても、祖先の設定で決める。
#[test]
fn ts_strip_internal_reads_resets_and_removed_directories() {
    assert_eq!(
        bucket(
            &[
                (
                    "config/a.json",
                    "{ \"compilerOptions\": { \"stripInternal\": true } }\n"
                ),
                (
                    "config/b.json",
                    "{ \"compilerOptions\": { \"stripInternal\": null } }\n"
                ),
                (
                    "tsconfig.json",
                    "{ \"extends\": [\"./config/a.json\", \"./config/b.json\"], \"compilerOptions\": { \"declaration\": true } }\n"
                ),
                ("src/lib.ts", OPTIONS)
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options"
        ),
        "none"
    );
    assert_eq!(
        bucket_after(
            &[
                ("tsconfig.json", STRIP),
                ("src/main.ts", "export const main = 1;\n"),
                (
                    "src/obsolete/only.ts",
                    "/** @internal */\nexport function helper(): void {}\n"
                )
            ],
            &[],
            &["src/obsolete/only.ts"],
            "helper"
        ),
        "none"
    );
}

/// 式の中の型 (アロー関数の引数など) は、宣言ファイルでは推論した型として出るので `@internal` の
/// メンバーも残る。宣言に書いた型は消える。
#[test]
fn ts_internal_members_in_inferred_types_stay_in_declarations() {
    let arrow = |ty: &str| {
        format!(
            "export const f = (v: {{\n  /** @internal */\n  x: {ty};\n  y: string;\n}}): void => {{}};\n"
        )
    };
    assert_eq!(
        bucket(
            &[("tsconfig.json", STRIP), ("src/lib.ts", &arrow("number"))],
            &[("src/lib.ts", &arrow("string"))],
            "f"
        ),
        "mod"
    );
    // 型を書いた変数は、書いた型が宣言ファイルに出るので消える
    let annotated = |ty: &str| {
        format!(
            "export const f: (v: {{\n  /** @internal */\n  x: {ty};\n  y: string;\n}}) => void = () => {{}};\n"
        )
    };
    assert_eq!(
        bucket(
            &[
                ("tsconfig.json", STRIP),
                ("src/lib.ts", &annotated("number"))
            ],
            &[("src/lib.ts", &annotated("string"))],
            "f"
        ),
        "none"
    );
}

/// ラベル付きのタプルの要素は宣言ファイルから消えるが、mapped type の節は消えない。
#[test]
fn ts_internal_tuple_members_and_mapped_types_follow_the_declaration_emitter() {
    let files = |lib: &'static str| [("tsconfig.json", STRIP), ("src/lib.ts", lib)];
    assert_eq!(
        bucket(
            &files("export type Args = [\n  value: string,\n  hidden: number\n];\n"),
            &[(
                "src/lib.ts",
                "export type Args = [\n  value: string,\n  /** @internal */\n  hidden: number\n];\n"
            )],
            "Args"
        ),
        "mod"
    );
    assert_eq!(
        bucket(
            &files("export type M<T> = {\n  /** @internal */\n  [K in keyof T]: number;\n};\n"),
            &[(
                "src/lib.ts",
                "export type M<T> = {\n  /** @internal */\n  [K in keyof T]: string;\n};\n"
            )],
            "M"
        ),
        "mod"
    );
}

/// ファイルを含まない別の用途の設定は、入力から import で届かなければ数えない。宣言ファイルに残る
/// 公開の関数の削除を見逃さない。
#[test]
fn ts_strip_internal_ignores_configs_that_do_not_reach_the_file() {
    assert_eq!(
        bucket(
            &[
                (
                    "tsconfig.json",
                    "{ \"compilerOptions\": { \"declaration\": true, \"stripInternal\": false }, \"files\": [\"src/lib.ts\", \"src/use.ts\"] }\n"
                ),
                (
                    "tsconfig.tools.json",
                    "{ \"compilerOptions\": { \"declaration\": true, \"stripInternal\": true }, \"files\": [\"tools/cli.ts\"] }\n"
                ),
                ("tools/cli.ts", "export const cli = 1;\n"),
                ("src/use.ts", "import { f } from \"./lib\";\nf();\n"),
                (
                    "src/lib.ts",
                    "/** @internal */\nexport function f(): void {}\n"
                )
            ],
            &[("src/lib.ts", "export {};\n")],
            "f"
        ),
        "rm"
    );
}

/// 互換の証明が読み込んだ内容と照合するときは、型のメンバーを除く前の元のソースで比べる。
#[test]
fn ts_strip_internal_keeps_source_checks_of_compatibility_proofs() {
    let lib = |params: &str| {
        format!(
            "export interface Hidden {{\n  /** @internal */\n  secret: number;\n  shown: string;\n}}\n\nexport function f({params}): string {{\n  return x;\n}}\n"
        )
    };
    let api = changed_api(
        &[
            ("tsconfig.json", STRIP),
            ("src/lib.ts", &lib("x: string, option?: boolean")),
            (
                "src/use.ts",
                "import { f } from \"./lib\";\n\nexport const result = f(\"ok\");\n",
            ),
        ],
        &[("src/lib.ts", &lib("x: string"))],
        &[],
    );
    assert!(
        !api.modified.iter().any(|change| change.name == "f"),
        "{api:?}"
    );
    assert!(
        api.compatible_modified
            .iter()
            .any(|change| change.name == "f" && change.reason == "trailing_optional_params_removed"),
        "{api:?}"
    );
}

/// 名前の無い `export default` のクラスは宣言なので、そのメンバーの `@internal` は宣言ファイルから
/// 消える。ラベルの無いタプルの要素と、`typeof` の型引数の型のメンバーも消える。
#[test]
fn ts_internal_tags_in_default_classes_tuples_and_type_queries() {
    let files = |lib: &'static str| [("tsconfig.json", STRIP), ("src/lib.ts", lib)];
    assert_eq!(
        bucket(
            &files("export default class {\n  open(): void {}\n  close(): void {}\n}\n"),
            &[(
                "src/lib.ts",
                "export default class {\n  open(): void {}\n  /** @internal */\n  close(): void {}\n}\n"
            )],
            "close"
        ),
        "rm_dead"
    );
    assert_eq!(
        bucket(
            &files("export type Args = [\n  string,\n  number\n];\n"),
            &[(
                "src/lib.ts",
                "export type Args = [\n  string,\n  /** @internal */\n  number\n];\n"
            )],
            "Args"
        ),
        "mod"
    );
    assert_eq!(
        bucket(
            &files(
                "declare function f<T>(x: T): T;\nexport type F = typeof f<{\n  x: number;\n  y: string;\n}>;\n"
            ),
            &[(
                "src/lib.ts",
                "declare function f<T>(x: T): T;\nexport type F = typeof f<{\n  /** @internal */\n  x: number;\n  y: string;\n}>;\n"
            )],
            "F"
        ),
        "mod"
    );
}

const API: &str = "/** @internal */\nexport function f(): void {}\n\nexport interface Options {\n  name: string;\n  /** @internal */\n  retries: number;\n}\n";

/// Issue #287 の再現: 設定だけを変えて `stripInternal` を有効にすると、変えていないソースの
/// `@internal` の宣言とメンバーが宣言ファイルから消える。
#[test]
fn ts_strip_internal_config_changes_recheck_unchanged_sources() {
    let files = [("tsconfig.json", NO_STRIP), ("src/api.ts", API)];
    assert_eq!(bucket(&files, &[("tsconfig.json", STRIP)], "f"), "rm_dead");
    assert_eq!(
        bucket(&files, &[("tsconfig.json", STRIP)], "Options"),
        "mod"
    );
    // `extends` の先の設定だけを変える
    let base = "{ \"extends\": \"./tsconfig.base.json\" }\n";
    let files = [
        ("tsconfig.json", base),
        ("tsconfig.base.json", NO_STRIP),
        ("src/api.ts", API),
    ];
    assert_eq!(
        bucket(&files, &[("tsconfig.base.json", STRIP)], "f"),
        "rm_dead"
    );
    // 設定を変えなければ比べ直さない
    let files = [("tsconfig.json", STRIP), ("src/api.ts", API)];
    assert_eq!(
        bucket(
            &files,
            &[("src/other.ts", "export const other = 1;\n")],
            "f"
        ),
        "none"
    );
}

/// 旧版のソースは旧版の設定で読む: 同じ差分で設定とソースを変えると、旧版の面は旧版の設定で
/// 作る。
#[test]
fn ts_strip_internal_reads_old_sources_with_old_configs() {
    let api = changed_api(
        &[("tsconfig.json", STRIP), ("src/api.ts", API)],
        &[
            ("tsconfig.json", NO_STRIP),
            ("src/api.ts", &format!("{API}\nexport const extra = 1;\n")),
        ],
        &[],
    );
    assert!(api.added.iter().any(|symbol| symbol.name == "f"), "{api:?}");
    assert!(
        api.modified.iter().any(|change| change.name == "Options"),
        "{api:?}"
    );
}

/// 内容が同じ rename でも、旧新のパスに当たる設定が違えば比べる。
#[test]
fn ts_strip_internal_renames_between_configs_are_compared() {
    let api = changed_api(
        &[
            ("packages/a/tsconfig.json", STRIP),
            ("packages/b/tsconfig.json", NO_STRIP),
            ("packages/b/src/api.ts", API),
            ("packages/a/src/keep.ts", "export const keep = 1;\n"),
        ],
        &[("packages/a/src/api.ts", API)],
        &["packages/b/src/api.ts"],
    );
    assert!(
        api.removed_dead.iter().any(|symbol| symbol.name == "f")
            || api.removed.iter().any(|symbol| symbol.name == "f"),
        "{api:?}"
    );
}

/// 設定を読めないときは、確定した差分にせず申告する。
#[test]
fn ts_strip_internal_unreadable_configs_are_reported() {
    let api = changed_api(
        &[("tsconfig.json", NO_STRIP), ("src/api.ts", API)],
        &[("tsconfig.json", "{ \"compilerOptions\": ")],
        &[],
    );
    assert!(
        api.removed_dead.iter().all(|symbol| symbol.name != "f"),
        "{api:?}"
    );
    assert!(
        api.parse_truncations
            .iter()
            .any(|truncation| truncation.reason
                == crate::models::truncation::TruncationReason::TsStripInternalUnavailable),
        "{api:?}"
    );
}

/// 設定ファイルだけを動かす (内容が同じ rename) 差分も、比べ直しの引き金になる。
#[test]
fn ts_strip_internal_config_renames_trigger_rechecks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("packages/a/tsconfig.json", STRIP),
            ("packages/a/src/keep.ts", "export const keep = 1;\n"),
            ("packages/b/src/api.ts", API),
        ],
        "base",
    );
    git_in(
        repo,
        &["mv", "packages/a/tsconfig.json", "packages/b/tsconfig.json"],
    );
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.removed_dead.iter().any(|symbol| symbol.name == "f"),
        "{api:?}"
    );
}

/// 生成物は、設定だけの変更でも比べ直さない (通常の差分と同じく外す)。
#[test]
fn ts_strip_internal_rechecks_skip_generated_files() {
    let api = changed_api(
        &[
            ("tsconfig.json", NO_STRIP),
            (".gitattributes", "src/gen.ts linguist-generated\n"),
            ("src/gen.ts", API),
        ],
        &[("tsconfig.json", STRIP)],
        &[],
    );
    assert!(
        api.removed_dead.iter().all(|symbol| symbol.name != "f")
            && api.removed.iter().all(|symbol| symbol.name != "f")
            && api.modified.iter().all(|change| change.name != "Options"),
        "{api:?}"
    );
}

/// 変えたソースでも、片方の設定を読めなければ、読めた側の判定を両方に当て (設定が変わったかを
/// 確かめられない)、申告する。
#[test]
fn ts_strip_internal_unreadable_side_uses_the_known_setting() {
    let api = changed_api(
        &[("tsconfig.json", STRIP), ("src/api.ts", API)],
        &[
            ("tsconfig.json", "{ \"compilerOptions\": "),
            ("src/api.ts", &format!("{API}\n")),
        ],
        &[],
    );
    assert!(
        api.modified.iter().all(|change| change.name != "Options")
            && api.added.iter().all(|symbol| symbol.name != "f"),
        "{api:?}"
    );
    assert!(
        api.parse_truncations
            .iter()
            .any(|truncation| truncation.reason
                == crate::models::truncation::TruncationReason::TsStripInternalUnavailable),
        "{api:?}"
    );
}

/// 大文字小文字を区別しないファイルシステムでは、base の版の設定も綴りの違いを同じパスとみなす。
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn ts_strip_internal_base_configs_follow_case_insensitive_paths() {
    let files = [
        (
            "tsconfig.json",
            "{ \"extends\": \"./TSConfig.base.json\" }\n",
        ),
        ("tsconfig.base.json", NO_STRIP),
        ("src/api.ts", API),
    ];
    assert_eq!(
        bucket(&files, &[("tsconfig.base.json", STRIP)], "f"),
        "rm_dead"
    );
}

/// 旧新とも設定を決められなければ、どちらも消さないとみなす (片方だけが有効と読めても、変化を
/// 結論しない)。旧版にだけ `@internal` がある (最後のタグを消した) 変更でも、決められた側の設定を
/// 旧版に当てる。
#[test]
fn ts_strip_internal_alignment_covers_both_unknown_and_removed_tags() {
    let api = changed_api(
        &[
            ("tsconfig.json", "{ \"compilerOptions\": "),
            ("src/api.ts", API),
        ],
        &[
            (
                "tsconfig.json",
                "{ \"extends\": \"./missing.json\", \"compilerOptions\": { \"stripInternal\": true } }\n",
            ),
            ("src/api.ts", &format!("{API}\n")),
        ],
        &[],
    );
    assert!(
        api.modified.iter().all(|change| change.name != "Options")
            && api.removed_dead.iter().all(|symbol| symbol.name != "f"),
        "{api:?}"
    );
    let api = changed_api(
        &[
            ("tsconfig.json", "{ \"compilerOptions\": "),
            (
                "src/api.ts",
                "/** @internal */\nexport function f(): void {}\n",
            ),
        ],
        &[
            ("tsconfig.json", STRIP),
            ("src/api.ts", "export function f(): void {}\n"),
        ],
        &[],
    );
    assert!(api.added.iter().any(|symbol| symbol.name == "f"), "{api:?}");
}

/// 入口だけを `files` に書いた出力の設定。
const BUILD: &str = "{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": { \"stripInternal\": true }, \"files\": [\"src/index.ts\"] }\n";

/// `src/lib.ts` の `retries` に `@internal` を足したときの `Options` の区分 (`index` が入口)。取り込み
/// を確かめられる構成なので、決められない申告が無いことも確かめる。
fn bucket_with_entry(files: &[(&str, &str)], index: &str) -> &'static str {
    let mut all = vec![("src/index.ts", index), ("src/lib.ts", OPTIONS)];
    all.extend_from_slice(files);
    let api = changed_api(&all, &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)], &[]);
    assert!(
        api.parse_truncations
            .iter()
            .all(|truncation| truncation.reason
                != crate::models::truncation::TruncationReason::TsStripInternalUnavailable),
        "{api:?}"
    );
    classify(&api, "Options")
}

/// Issue #288 の再現: 入力から import で取り込むファイルも、その設定が宣言を出すファイルとして
/// `stripInternal` を当てる。TypeScript と同じ解決で、実際に選ぶファイルを辿る。
#[test]
fn ts_strip_internal_follows_imports_from_entry_files() {
    let base = [("tsconfig.json", NO_STRIP), ("tsconfig.build.json", BUILD)];
    // 取り込みの先の先 (ディレクトリの index と `import` の再公開)
    assert_eq!(
        bucket_with_entry(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", BUILD),
                ("src/api/index.ts", "export * from \"../lib\";\n")
            ],
            "import { connect } from \"./api\";\nexport { connect };\n"
        ),
        "mod"
    );
    // `/// <reference path>` の先
    assert_eq!(
        bucket_with_entry(&base, "/// <reference path=\"./lib.ts\" />\nexport {};\n"),
        "mod"
    );
    // 型の位置の `import()` の先
    assert_eq!(
        bucket_with_entry(
            &base,
            "export type Connect = typeof import(\"./lib\").connect;\n"
        ),
        "mod"
    );
    // node16 系の ESM では `.js` の指定子を `.ts` に置き換えて解く
    let esm = "{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": { \"stripInternal\": true, \"module\": \"nodenext\" }, \"files\": [\"src/index.ts\"] }\n";
    let esm_files = [
        ("package.json", "{ \"type\": \"module\" }\n"),
        ("tsconfig.json", NO_STRIP),
        ("tsconfig.build.json", esm),
    ];
    assert_eq!(
        bucket_with_entry(&esm_files, "export * from \"./lib.js\";\n"),
        "mod"
    );
    // 対照: ESM では拡張子を補わないので、拡張子の無い指定子は解けず取り込まない
    assert_eq!(
        bucket_with_entry(&esm_files, "export * from \"./lib\";\n"),
        "none"
    );
}

/// 取り込みを変えるだけの差分でも、差分に無いソースの判定が変われば比べ直す。旧版は旧版の
/// 取り込みで判定する。
#[test]
fn ts_strip_internal_import_changes_recheck_unchanged_sources() {
    let files = [
        ("tsconfig.json", NO_STRIP),
        ("tsconfig.build.json", BUILD),
        ("src/index.ts", "export const version = 1;\n"),
        ("src/api.ts", API),
    ];
    let reexport = "export const version = 1;\nexport * from \"./api\";\n";
    assert!(
        matches!(
            bucket(&files, &[("src/index.ts", reexport)], "f"),
            "rm" | "rm_dead"
        ),
        "{:?}",
        changed_api(&files, &[("src/index.ts", reexport)], &[])
    );
    assert_eq!(
        bucket(&files, &[("src/index.ts", reexport)], "Options"),
        "mod"
    );
    // 取り込みを変えなければ比べ直さない
    assert_eq!(
        bucket(
            &files,
            &[("src/index.ts", "export const version = 2;\n")],
            "f"
        ),
        "none"
    );
}

/// `allowJs` の無い設定は JS を取り込まず、その先を辿らない。`allowJs` なら JS の `require()` も
/// 辿る。
#[test]
fn ts_strip_internal_follows_javascript_only_with_allow_js() {
    let build = |allow_js: bool| {
        format!(
            "{{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": {{ \"stripInternal\": true, \"allowJs\": {allow_js} }}, \"files\": [\"src/index.ts\"] }}\n"
        )
    };
    let files = |build: &str| {
        bucket_with_entry(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", build),
                ("src/bridge.js", "module.exports = require(\"./lib\");\n"),
            ],
            "export * from \"./bridge.js\";\n",
        )
    };
    assert_eq!(files(&build(false)), "none");
    assert_eq!(files(&build(true)), "mod");
}

/// 取り込みで届く近い設定は、祖先の出力の設定より先に判定を決める。
#[test]
fn ts_strip_internal_nearest_config_that_imports_the_file_decides() {
    let src = "{ \"compilerOptions\": { \"declaration\": true }, \"files\": [\"index.ts\"] }\n";
    let files = [("tsconfig.json", STRIP), ("src/tsconfig.json", src)];
    assert_eq!(
        bucket_with_entry(&files, "export * from \"./lib\";\n"),
        "none"
    );
    // 対照: 取り込まなければ、祖先の出力の設定で決める
    assert_eq!(bucket_with_entry(&files, "export const x = 1;\n"), "mod");
}

/// `exclude` を書かなければ、`outDir` の下は入力に含まない (TypeScript の既定の除外)。
#[test]
fn ts_strip_internal_excludes_out_dir_by_default() {
    let files = |config: &str| {
        bucket(
            &[("tsconfig.json", config), ("out/lib.ts", OPTIONS)],
            &[("out/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            "Options",
        )
    };
    assert_eq!(
        files(
            "{ \"compilerOptions\": { \"declaration\": true, \"stripInternal\": true, \"outDir\": \"out\" } }\n"
        ),
        "none"
    );
    // 対照: `exclude` を書けば、`outDir` の下も入力になる
    assert_eq!(
        files(
            "{ \"compilerOptions\": { \"declaration\": true, \"stripInternal\": true, \"outDir\": \"out\" }, \"exclude\": [] }\n"
        ),
        "mod"
    );
}

/// 判定を決められなかったときの申告。
fn strip_unavailable(
    api: &crate::models::review::ApiChanges,
) -> Vec<crate::models::truncation::TruncationInfo> {
    api.parse_truncations
        .iter()
        .filter(|truncation| {
            truncation.reason
                == crate::models::truncation::TruncationReason::TsStripInternalUnavailable
        })
        .cloned()
        .collect()
}

/// 自身の名前 (`exports` のある package.json の `name`) で取り込むと、どのファイルに解かれるかを
/// 確かめないので、判定を決められないと申告する。この差分で付けた `@internal` は、消える場合に
/// 備えて削除として出す。設定の変更で比べ直せないソースは、まとめて申告する。
#[test]
fn ts_strip_internal_unresolved_imports_are_reported() {
    let package = "{ \"name\": \"pkg\", \"exports\": { \"./*\": \"./src/*.ts\" } }\n";
    let api = changed_api(
        &[
            ("package.json", package),
            ("tsconfig.json", NO_STRIP),
            ("tsconfig.build.json", BUILD),
            ("src/index.ts", "export * from \"pkg/lib\";\n"),
            ("src/lib.ts", OPTIONS),
        ],
        &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
        &[],
    );
    assert!(
        api.modified.iter().any(|change| change.name == "Options"),
        "{api:?}"
    );
    assert!(!strip_unavailable(&api).is_empty(), "{api:?}");
    // 設定だけの変更: 差分に無い `src/api.ts` は比べ直せないので、まとめて申告する
    let api = changed_api(
        &[
            ("package.json", package),
            ("tsconfig.json", NO_STRIP),
            ("tsconfig.build.json", BUILD),
            ("src/index.ts", "export * from \"pkg/api\";\n"),
            ("src/api.ts", API),
        ],
        &[(
            "tsconfig.json",
            "{ \"compilerOptions\": { \"declaration\": true, \"strict\": true } }\n",
        )],
        &[],
    );
    assert!(
        api.removed_dead.iter().all(|symbol| symbol.name != "f"),
        "{api:?}"
    );
    assert!(
        strip_unavailable(&api).iter().any(
            |truncation| truncation.path.is_none() && truncation.message.contains("src/api.ts")
        ),
        "{api:?}"
    );
}

/// `paths` と `baseUrl` は TypeScript と同じ順に解く (当たったパターンの置換先、無ければ
/// `baseUrl` の下)。
#[test]
fn ts_strip_internal_follows_paths_and_base_url() {
    let config = |options: &str| {
        format!(
            "{{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": {{ \"stripInternal\": true, {options} }}, \"files\": [\"src/index.ts\"] }}\n"
        )
    };
    let paths = config("\"paths\": { \"@/*\": [\"./src/*\"] }");
    assert_eq!(
        bucket_with_entry(
            &[("tsconfig.json", NO_STRIP), ("tsconfig.build.json", &paths)],
            "export * from \"@/lib\";\n"
        ),
        "mod"
    );
    let base_url = config("\"baseUrl\": \"./src\"");
    assert_eq!(
        bucket_with_entry(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", &base_url)
            ],
            "export * from \"lib\";\n"
        ),
        "mod"
    );
    // 対照: どれにも当たらない指定子は外のライブラリ (宣言を出すファイルを足さない)
    assert_eq!(
        bucket_with_entry(
            &[("tsconfig.json", NO_STRIP), ("tsconfig.build.json", &paths)],
            "export * from \"react\";\n"
        ),
        "none"
    );
}

/// モジュールの拡張 (`declare module "./lib"`) は名前を解くだけで、ファイルを取り込まない。
#[test]
fn ts_strip_internal_augmentations_do_not_reach_files() {
    assert_eq!(
        bucket_with_entry(
            &[("tsconfig.json", NO_STRIP), ("tsconfig.build.json", BUILD)],
            "export {};\ndeclare module \"./lib\" {\n  interface Extra {}\n}\n"
        ),
        "none"
    );
}

/// 空のファイルを消すと指定子の解決先が変わる (`./bridge` が `bridge.ts` から `bridge.js` に)。
/// hunk の無い削除でも比べ直す。
#[test]
fn ts_strip_internal_empty_file_changes_recheck_unchanged_sources() {
    let build = "{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": { \"stripInternal\": true, \"allowJs\": true }, \"files\": [\"src/index.ts\"] }\n";
    let files = [
        ("tsconfig.json", NO_STRIP),
        ("tsconfig.build.json", build),
        ("src/index.ts", "import \"./bridge\";\n"),
        ("src/bridge.ts", ""),
        ("src/bridge.js", "module.exports = require(\"./api\");\n"),
        ("src/api.ts", API),
    ];
    assert!(
        matches!(
            bucket_after(&files, &[], &["src/bridge.ts"], "f"),
            "rm" | "rm_dead"
        ),
        "{:?}",
        changed_api(&files, &[], &["src/bridge.ts"])
    );
}

/// `include` のワイルドカードの `*` は `.min.js` に当たらない (書けば当たる)。`jsconfig.json` は
/// `allowJs` を既定で有効にする。
#[test]
fn ts_strip_internal_lists_javascript_inputs_like_typescript() {
    let build = |include: &str| {
        format!(
            "{{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": {{ \"stripInternal\": true, \"allowJs\": true }}, \"include\": [\"{include}\"] }}\n"
        )
    };
    let files = |build: &str| {
        bucket_with_entry(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", build),
                ("entry/loader.min.js", "require(\"../src/lib\");\n"),
            ],
            "export {};\n",
        )
    };
    assert_eq!(files(&build("entry/*")), "none");
    assert_eq!(files(&build("entry/*.min.js")), "mod");
    assert_eq!(
        bucket_with_entry(
            &[
                ("tsconfig.json", NO_STRIP),
                (
                    "jsconfig.json",
                    "{ \"compilerOptions\": { \"stripInternal\": true }, \"files\": [\"src/main.js\"] }\n"
                ),
                ("src/main.js", "require(\"./lib\");\n"),
            ],
            "export {};\n"
        ),
        "mod"
    );
}

/// 旧版の判定を決められず新版は消すとき、ソースから消した `@internal` の宣言の削除は隠さない。
/// 旧新とも決められなければ、この差分で付けた `@internal` を削除として出す (元からの
/// `@internal` の項目は比べる)。
#[test]
fn ts_strip_internal_undetermined_sides_keep_source_changes() {
    // 旧版の入口は解析できない (取り込みを決められない)。新版の入口は `./api` を取り込む
    let files = [
        ("tsconfig.json", NO_STRIP),
        ("tsconfig.build.json", BUILD),
        ("src/index.ts", "export const broken = ;\n"),
        ("src/api.ts", API),
    ];
    let without_f =
        "export interface Options {\n  name: string;\n  /** @internal */\n  retries: number;\n}\n";
    let api = changed_api(
        &files,
        &[
            ("src/index.ts", "export * from \"./api\";\n"),
            ("src/api.ts", without_f),
        ],
        &[],
    );
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "f")
            || api.removed_dead.iter().any(|symbol| symbol.name == "f"),
        "{api:?}"
    );
    // 元から `@internal` で変えていないメンバーは、判定の違いだけで変化にしない
    assert!(
        api.modified.iter().all(|change| change.name != "Options"),
        "{api:?}"
    );
    assert!(!strip_unavailable(&api).is_empty(), "{api:?}");
    // 旧新とも決められない: `@internal` を付けた export は削除として出す
    let files = [
        ("tsconfig.json", NO_STRIP),
        ("tsconfig.build.json", BUILD),
        ("src/index.ts", "export const broken = ;\n"),
        ("src/lib.ts", OPTIONS),
    ];
    let api = changed_api(
        &files,
        &[
            ("src/index.ts", "export const broken = ;\nexport {};\n"),
            ("src/lib.ts", OPTIONS_INTERNAL_EXPORT),
        ],
        &[],
    );
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "connect")
            || api
                .removed_dead
                .iter()
                .any(|symbol| symbol.name == "connect"),
        "{api:?}"
    );
}

/// 判定を決められない側の項目は宣言ごとに対応させる: 変数の文の宣言を減らしても、元から
/// `@internal` の宣言を付け足しとはみなさない。文字列の中の空白の変化はソースの変化として扱う。
#[test]
fn ts_strip_internal_undetermined_items_follow_bindings_and_literals() {
    // 旧版は入口が届かないと確かめられ (消さない)、新版は入口を解析できない (決められない)
    let api = changed_api(
        &[
            ("tsconfig.json", NO_STRIP),
            ("tsconfig.build.json", BUILD),
            ("src/index.ts", "export const ok = 1;\n"),
            (
                "src/lib.ts",
                "/** @internal */\nexport const a = 1, b = 2;\nexport const c = 3;\n",
            ),
        ],
        &[
            ("src/index.ts", "export const broken = ;\n"),
            (
                "src/lib.ts",
                "/** @internal */\nexport const a = 1;\nexport const c = 3;\n",
            ),
        ],
        &[],
    );
    let removed = |name: &str| {
        api.removed.iter().any(|symbol| symbol.name == name)
            || api.removed_dead.iter().any(|symbol| symbol.name == name)
    };
    assert!(removed("b"), "{api:?}");
    assert!(!removed("a"), "{api:?}");
    // 宣言を足しても、元から `@internal` の宣言を付け足しとはみなさない
    let api = changed_api(
        &[
            ("tsconfig.json", NO_STRIP),
            ("tsconfig.build.json", BUILD),
            ("src/index.ts", "export const ok = 1;\n"),
            ("src/lib.ts", "/** @internal */\nexport const a = 1;\n"),
        ],
        &[
            ("src/index.ts", "export const broken = ;\n"),
            (
                "src/lib.ts",
                "/** @internal */\nexport const a = 1, b = 2;\n",
            ),
        ],
        &[],
    );
    assert!(
        api.removed.iter().all(|symbol| symbol.name != "a")
            && api.removed_dead.iter().all(|symbol| symbol.name != "a"),
        "{api:?}"
    );
    // 旧版を決められず新版は消す: `return` の後の改行で式が切れる変化 (構造の変化) も比べる
    let api = changed_api(
        &[
            ("tsconfig.json", NO_STRIP),
            ("tsconfig.build.json", BUILD),
            ("src/index.ts", "export const broken = ;\n"),
            (
                "src/lib.ts",
                "/** @internal */\nexport function f() {\n  return \"ok\";\n}\nexport const c = 3;\n",
            ),
        ],
        &[
            ("src/index.ts", "export * from \"./lib\";\n"),
            (
                "src/lib.ts",
                "/** @internal */\nexport function f() {\n  return\n  \"ok\";\n}\nexport const c = 3;\n",
            ),
        ],
        &[],
    );
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "f")
            || api.removed_dead.iter().any(|symbol| symbol.name == "f")
            || api.modified.iter().any(|change| change.name == "f"),
        "{api:?}"
    );
    // 旧版を決められず新版は消す: `@internal` の型の文字列の中身を変えたら、残したまま比べる
    let api = changed_api(
        &[
            ("tsconfig.json", NO_STRIP),
            ("tsconfig.build.json", BUILD),
            ("src/index.ts", "export const broken = ;\n"),
            (
                "src/lib.ts",
                "/** @internal */\nexport type T = \"a b\";\nexport const c = 3;\n",
            ),
        ],
        &[
            ("src/index.ts", "export * from \"./lib\";\n"),
            (
                "src/lib.ts",
                "/** @internal */\nexport type T = \"ab\";\nexport const c = 3;\n",
            ),
        ],
        &[],
    );
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "T")
            || api.removed_dead.iter().any(|symbol| symbol.name == "T")
            || api.modified.iter().any(|change| change.name == "T"),
        "{api:?}"
    );
}

/// `paths` の置換先の拡張子は、`*` を置き換える前の値で見る (`@/lib.js` は `lib.ts` を先に探す)。
#[test]
fn ts_strip_internal_paths_check_extensions_before_substitution() {
    let paths = "{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": { \"stripInternal\": true, \"paths\": { \"@/*\": [\"./src/*\"] } }, \"files\": [\"src/index.ts\"] }\n";
    assert_eq!(
        bucket_with_entry(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", paths),
                ("src/lib.js", "module.exports = {};\n"),
            ],
            "export * from \"@/lib.js\";\n"
        ),
        "mod"
    );
}

/// 合成の取り込み (`importHelpers` の tslib) は、TypeScript がモジュールに足すときだけ辿る。
/// モジュールかを決められないスクリプトでは、足し得る先があれば決めない。
#[test]
fn ts_strip_internal_synthetic_imports_follow_module_detection() {
    let build = |detection: &str| {
        format!(
            "{{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": {{ \"stripInternal\": true, \"importHelpers\": true, \"paths\": {{ \"tslib\": [\"./src/lib.ts\"] }}{detection} }}, \"files\": [\"src/index.ts\"] }}\n"
        )
    };
    let api = |build: &str, index: &str| {
        changed_api(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", build),
                ("src/index.ts", index),
                ("src/lib.ts", OPTIONS),
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            &[],
        )
    };
    // モジュールには足す (届く)
    let module = api(&build(""), "export const x = 1;\n");
    assert_eq!(classify(&module, "Options"), "mod", "{module:?}");
    assert!(strip_unavailable(&module).is_empty(), "{module:?}");
    // `moduleDetection: legacy` のスクリプトには足さない (届かない)
    let legacy = api(
        &build(", \"moduleDetection\": \"legacy\""),
        "const x = 1;\n",
    );
    assert_eq!(classify(&legacy, "Options"), "none", "{legacy:?}");
    assert!(strip_unavailable(&legacy).is_empty(), "{legacy:?}");
    // 既定の `moduleDetection` のスクリプトは決めない
    let auto = api(&build(""), "const x = 1;\n");
    assert!(!strip_unavailable(&auto).is_empty(), "{auto:?}");
    // `import.meta` を使うソースは `legacy` でもモジュール (足す)
    let meta = api(
        &build(", \"moduleDetection\": \"legacy\""),
        "void import.meta;\n",
    );
    assert_eq!(classify(&meta, "Options"), "mod", "{meta:?}");
    assert!(strip_unavailable(&meta).is_empty(), "{meta:?}");
}

/// JSX の runtime の取り込みは、ソースの pragma (`@jsxRuntime classic` など) で変わるので、pragma の
/// あるソースからは届くと決めない。同じ宣言ファイルで宣言した ambient module の名前の取り込みは、
/// ファイルを探さない。
#[test]
fn ts_strip_internal_jsx_pragmas_and_own_ambient_modules() {
    let build = "{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": { \"stripInternal\": true, \"jsx\": \"react-jsx\", \"paths\": { \"react/jsx-runtime\": [\"./src/lib.ts\"] } }, \"files\": [\"src/index.ts\"] }\n";
    let api = |index: &str| {
        changed_api(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", build),
                ("src/index.ts", index),
                ("src/lib.ts", OPTIONS),
            ],
            &[("src/lib.ts", OPTIONS_INTERNAL_MEMBER)],
            &[],
        )
    };
    let module = api("export {};\n");
    assert_eq!(classify(&module, "Options"), "mod", "{module:?}");
    assert!(strip_unavailable(&module).is_empty(), "{module:?}");
    let pragma = api("/** @jsxRuntime classic */\nexport {};\n");
    assert!(!strip_unavailable(&pragma).is_empty(), "{pragma:?}");
    // 同じ宣言ファイルの ambient module の名前は `paths` で解かない
    let entry = "{ \"extends\": \"./tsconfig.json\", \"compilerOptions\": { \"stripInternal\": true, \"paths\": { \"pkg\": [\"./src/lib.ts\"] } }, \"files\": [\"src/entry.d.ts\"] }\n";
    assert_eq!(
        bucket_with_entry(
            &[
                ("tsconfig.json", NO_STRIP),
                ("tsconfig.build.json", entry),
                (
                    "src/entry.d.ts",
                    "declare module \"use\" {\n  export { X } from \"pkg\";\n}\ndeclare module \"pkg\" {\n  export interface X {}\n}\n"
                ),
            ],
            "export {};\n"
        ),
        "none"
    );
}
