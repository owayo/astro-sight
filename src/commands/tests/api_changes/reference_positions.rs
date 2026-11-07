//! 参照の構文上の位置と束縛で判定する降格 (mod_closed / mod_compat / removed_dead) のテスト。
//!
//! 名前で集めた参照を行やファイルの単位で扱うと、import 文と同じ行の呼び出し、別名 import の
//! 利用、default export の利用者、`.vue` の中の利用を見落として非 blocking に落ちていた。
//! 各テストは見落とす入力と、従来どおり降格してよい入力 (対照) を並べる。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::ApiChanges;
#[allow(unused_imports)]
use std::fs;

/// base の `files` をコミットし、作業ツリーへ `changes` を書いてから API 差分を取る。
fn api_after(files: &[(&str, &str)], changes: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, files, "base");
    for (path, content) in changes {
        let full = repo.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(full, content).expect("write");
    }
    detect_api_changes_from_worktree(repo)
}

/// `name` がどの区分に入ったか (どこにも無ければ "none")。
fn bucket(api: &ApiChanges, name: &str) -> &'static str {
    if api.modified.iter().any(|c| c.name == name) {
        "mod"
    } else if api.modified_closed_in_diff.iter().any(|c| c.name == name) {
        "mod_closed"
    } else if api.compatible_modified.iter().any(|c| c.name == name) {
        "mod_compat"
    } else if api.removed.iter().any(|c| c.name == name) {
        "rm"
    } else if api.removed_dead.iter().any(|c| c.name == name) {
        "rm_dead"
    } else {
        "none"
    }
}

const TS_FOO_OLD: &str = "export function foo(x: number): number {\n  return x;\n}\n";
const TS_FOO_NEW: &str =
    "export function foo(x: number, y: number): number {\n  return x + y;\n}\n";
const TS_C_OLD: &str =
    "import { foo } from \"./a\";\n\nexport function viaC(): number {\n  return foo(1);\n}\n";
const TS_C_NEW: &str =
    "import { foo } from \"./a\";\n\nexport function viaC(): number {\n  return foo(1, 2);\n}\n";

/// import 文と同じ行に書いた未更新の呼び出しは、import の参照として捨てずに検査する
/// (行単位の除外で mod_closed に落ちていた)。同じ行の呼び出しも更新していれば従来どおり
/// mod_closed (対照)。
#[test]
fn ts_call_on_import_line_is_checked_for_closure() {
    let one_line_old =
        "import { foo } from \"./a\"; export function viaB(): number { return foo(1); }\n";
    let one_line_new =
        "import { foo } from \"./a\"; export function viaB(): number { return foo(1, 2); }\n";
    let api = api_after(
        &[
            ("src/a.ts", TS_FOO_OLD),
            ("src/b.ts", one_line_old),
            ("src/c.ts", TS_C_OLD),
        ],
        &[("src/a.ts", TS_FOO_NEW), ("src/c.ts", TS_C_NEW)],
    );
    assert_eq!(bucket(&api, "foo"), "mod", "{api:?}");
    let api = api_after(
        &[
            ("src/a.ts", TS_FOO_OLD),
            ("src/b.ts", one_line_old),
            ("src/c.ts", TS_C_OLD),
        ],
        &[
            ("src/a.ts", TS_FOO_NEW),
            ("src/b.ts", one_line_new),
            ("src/c.ts", TS_C_NEW),
        ],
    );
    assert_eq!(bucket(&api, "foo"), "mod_closed", "{api:?}");
}

const PY_FOO_OLD: &str = "def foo(x):\n    return x\n";
const PY_FOO_NEW: &str = "def foo(x, y):\n    return x + y\n";
const PY_C_OLD: &str = "from pkg.a import foo\n\n\ndef main():\n    return foo(1)\n";
const PY_C_NEW: &str = "from pkg.a import foo\n\n\ndef main():\n    return foo(1, 2)\n";

/// Python も同じ: `from m import foo; VALUE = foo(1)`、関数内の `from m import foo; return foo(1)`、
/// import 文の無い行 `use = foo(1)` (行頭 `use ` は Rust の構文) の未更新の呼び出しを捨てない。
#[test]
fn python_call_on_import_line_or_use_assignment_is_checked_for_closure() {
    for (label, old, updated) in [
        (
            "same-line import",
            "from pkg.a import foo; VALUE = foo(1)\n",
            "from pkg.a import foo; VALUE = foo(1, 2)\n",
        ),
        (
            "import inside function",
            "def main2():\n    from pkg.a import foo; return foo(1)\n",
            "def main2():\n    from pkg.a import foo; return foo(1, 2)\n",
        ),
        (
            "use assignment",
            "from pkg.a import foo\n\n\ndef main2():\n    use = foo(1)\n    return use\n",
            "from pkg.a import foo\n\n\ndef main2():\n    use = foo(1, 2)\n    return use\n",
        ),
    ] {
        let base = [
            ("pkg/__init__.py", ""),
            ("pkg/a.py", PY_FOO_OLD),
            ("pkg/b.py", old),
            ("pkg/c.py", PY_C_OLD),
        ];
        let api = api_after(&base, &[("pkg/a.py", PY_FOO_NEW), ("pkg/c.py", PY_C_NEW)]);
        assert_eq!(bucket(&api, "foo"), "mod", "{label}: {api:?}");
        // 対照: 同じ行の呼び出しも更新していれば mod_closed
        let api = api_after(
            &base,
            &[
                ("pkg/a.py", PY_FOO_NEW),
                ("pkg/b.py", updated),
                ("pkg/c.py", PY_C_NEW),
            ],
        );
        assert_eq!(bucket(&api, "foo"), "mod_closed", "{label}: {api:?}");
    }
}

/// 別名で import / 再 export した名前の利用 (`r(1)` / `execute(1)`) は名前索引に現れない。
/// 見えた呼び出しが全部更新済みでも mod_closed にしない (言語を問わない)。
#[test]
fn aliased_imports_and_reexports_keep_signature_change_blocking() {
    // Python の `from m import foo as f`
    let api = api_after(
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", PY_FOO_OLD),
            (
                "pkg/b.py",
                "from pkg.a import foo as f\n\n\ndef main2():\n    return f(1)\n",
            ),
            ("pkg/c.py", PY_C_OLD),
        ],
        &[("pkg/a.py", PY_FOO_NEW), ("pkg/c.py", PY_C_NEW)],
    );
    assert_eq!(bucket(&api, "foo"), "mod", "python alias: {api:?}");
    // TS の `import { foo as f }`
    let api = api_after(
        &[
            ("src/a.ts", TS_FOO_OLD),
            (
                "src/b.ts",
                "import { foo as f } from \"./a\";\nexport const v = f(1);\n",
            ),
            ("src/c.ts", TS_C_OLD),
        ],
        &[("src/a.ts", TS_FOO_NEW), ("src/c.ts", TS_C_NEW)],
    );
    assert_eq!(bucket(&api, "foo"), "mod", "ts alias import: {api:?}");
    // 別名の再 export は、その行を同じ diff で書き換えていても追随済みにならない
    let api = api_after(
        &[
            ("src/a.ts", TS_FOO_OLD),
            ("src/barrel.ts", "export { foo as execute } from \"./a\";\n"),
            (
                "src/d.ts",
                "import { execute } from \"./barrel\";\nexport const v = execute(1);\n",
            ),
            ("src/c.ts", TS_C_OLD),
        ],
        &[
            ("src/a.ts", TS_FOO_NEW),
            (
                "src/barrel.ts",
                "export { foo as execute } from \"./a.ts\";\n",
            ),
            ("src/c.ts", TS_C_NEW),
        ],
    );
    assert_eq!(bucket(&api, "foo"), "mod", "ts alias re-export: {api:?}");
}

const FORMAT_DEFAULT_OLD: &str = "export default function formatPrice(value: number): string {\n  return String(value);\n}\n\nexport function formatTotal(values: number[]): string {\n  return values.map((v) => formatPrice(v)).join(\",\");\n}\n";
const FORMAT_DEFAULT_NEW: &str = "export default function formatPrice(value: number, currency: string): string {\n  return currency + String(value);\n}\n\nexport function formatTotal(values: number[]): string {\n  return values.map((v) => formatPrice(v, \"$\")).join(\",\");\n}\n";

/// default export / 別名 export の関数は、利用者が宣言名と違う名前 (`import fmt from` /
/// `import { price } from`) で書くので、宣言名の参照が同じファイル内だけでも「同一ファイル内で
/// しか使われていない」とは言えない。シグネチャ変更を捨てずに api.mod に残す。
/// 宣言名のまま公開した関数の同じ変更は従来どおり捨てる (対照)。
#[test]
fn default_and_aliased_exports_are_not_internally_closed() {
    let api = api_after(
        &[
            ("src/format.ts", FORMAT_DEFAULT_OLD),
            (
                "src/use.ts",
                "import fmt from \"./format\";\nexport const label: string = fmt(10);\n",
            ),
        ],
        &[("src/format.ts", FORMAT_DEFAULT_NEW)],
    );
    assert_eq!(
        bucket(&api, "formatPrice"),
        "mod",
        "default export: {api:?}"
    );
    let alias_old = FORMAT_DEFAULT_OLD.replace("export default function", "function")
        + "export { formatPrice as price };\n";
    let alias_new = FORMAT_DEFAULT_NEW.replace("export default function", "function")
        + "export { formatPrice as price };\n";
    let api = api_after(
        &[
            ("src/format.ts", &alias_old),
            (
                "src/use.ts",
                "import { price } from \"./format\";\nexport const label: string = price(10);\n",
            ),
        ],
        &[("src/format.ts", &alias_new)],
    );
    assert_eq!(bucket(&api, "formatPrice"), "mod", "alias export: {api:?}");
    // 対照: 宣言名のまま export し、同じファイル内でしか呼ばれない関数の変更は従来どおり出さない
    let named_old = FORMAT_DEFAULT_OLD.replace("export default function", "export function");
    let named_new = FORMAT_DEFAULT_NEW.replace("export default function", "export function");
    let api = api_after(
        &[("src/format.ts", &named_old)],
        &[("src/format.ts", &named_new)],
    );
    assert_eq!(bucket(&api, "formatPrice"), "none", "named export: {api:?}");
}

/// default export を別の名前で default import した未更新の利用者は名前索引に現れないので、
/// 宣言名で import した呼び出しが全部更新済みでも mod_closed にしない。
#[test]
fn default_import_under_other_name_keeps_closure_blocking() {
    let old = "export default function formatPrice(value: number): string {\n  return String(value);\n}\n";
    let new = "export default function formatPrice(value: number, currency: string): string {\n  return currency + String(value);\n}\n";
    let api = api_after(
        &[
            ("src/format.ts", old),
            (
                "src/a.ts",
                "import formatPrice from \"./format\";\nexport const a = formatPrice(10);\n",
            ),
            (
                "src/b.ts",
                "import fmt from \"./format\";\nexport const b = fmt(20);\n",
            ),
        ],
        &[
            ("src/format.ts", new),
            (
                "src/a.ts",
                "import formatPrice from \"./format\";\nexport const a = formatPrice(10, \"$\");\n",
            ),
        ],
    );
    assert_eq!(bucket(&api, "formatPrice"), "mod", "{api:?}");
}

const FORMAT_FN_OLD: &str =
    "export function format(value: number): string {\n  return String(value);\n}\n";
const FORMAT_FN_NEW: &str = "export function format(value: number, unit: string): string {\n  return String(value) + unit;\n}\n";

/// `.vue` / `.mdx` のような解析できない埋め込みソースにある利用は refs に現れない。対象名が
/// 現れ得るソースがあれば、見えた呼び出しが全部更新済みでも mod_closed にしない。対象名が
/// 現れないなら従来どおり mod_closed (対照)。
#[test]
fn embedded_sources_mentioning_the_name_keep_closure_blocking() {
    for (path, user, mentions) in [
        (
            "src/App.vue",
            "<script setup lang=\"ts\">\nimport { format } from \"./format\";\nconst text = format(1);\n</script>\n<template><p>{{ text }}</p></template>\n",
            true,
        ),
        (
            "docs/usage.mdx",
            "import { format } from \"../src/format\";\n\n# Usage\n\n{format(1)}\n",
            true,
        ),
        (
            "src/App.vue",
            "<script setup lang=\"ts\">\nconst text = \"hello\";\n</script>\n<template><p>{{ text }}</p></template>\n",
            false,
        ),
    ] {
        let api = api_after(
            &[
                ("src/format.ts", FORMAT_FN_OLD),
                (
                    "src/main.ts",
                    "import { format } from \"./format\";\nexport const m = format(2);\n",
                ),
                (path, user),
            ],
            &[
                ("src/format.ts", FORMAT_FN_NEW),
                (
                    "src/main.ts",
                    "import { format } from \"./format\";\nexport const m = format(2, \"px\");\n",
                ),
            ],
        );
        let expected = if mentions { "mod" } else { "mod_closed" };
        assert_eq!(bucket(&api, "format"), expected, "{path}: {api:?}");
    }
}

/// `.vue` にある `config.b` は member access の走査に現れない。削除キーが現れ得る埋め込み
/// ソースがあれば unused_object_members (非 blocking) に降格しない。現れなければ従来どおり (対照)。
#[test]
fn embedded_sources_mentioning_removed_key_keep_member_removal_blocking() {
    for (vue, expected) in [
        (
            "<script setup lang=\"ts\">\nimport { config } from \"./config\";\nconst total = config.a + config.b;\n</script>\n<template><p>{{ total }}</p></template>\n",
            "mod",
        ),
        (
            "<script setup lang=\"ts\">\nimport { config } from \"./config\";\nconst total = config.a;\n</script>\n<template><p>{{ total }}</p></template>\n",
            "mod_compat",
        ),
    ] {
        let api = api_after(
            &[
                (
                    "src/config.ts",
                    "export const config = {\n  a: 1,\n  b: 2,\n};\n",
                ),
                (
                    "src/main.ts",
                    "import { config } from \"./config\";\nexport const v = config.a;\n",
                ),
                ("src/App.vue", vue),
            ],
            &[("src/config.ts", "export const config = {\n  a: 1,\n};\n")],
        );
        assert_eq!(bucket(&api, "config"), expected, "{vue}: {api:?}");
    }
}

/// JSX のタグ利用だけなら互換とする判定は、参照位置のノードで行う。256 バイトで切り詰めた
/// 行テキストで判定すると、長い行の後半にある直接呼び出し `{Banner()}` を見落としていた。
/// import 文と同じ行の直接呼び出しも捨てない。JSX だけの利用は従来どおり互換 (対照)。
#[test]
fn jsx_only_usage_is_judged_by_reference_position() {
    let card_old = "export function Banner() {\n  return <div>banner</div>;\n}\n";
    let card_new =
        "export function Banner({ tone }: { tone?: string }) {\n  return <div>{tone}</div>;\n}\n";
    let long_title = "t".repeat(300);
    let with_call = format!(
        "import {{ Banner }} from \"./card\";\nexport function View() {{\n  return <section><Banner /><p title=\"{long_title}\"></p>{{Banner()}}</section>;\n}}\n"
    );
    let jsx_only = format!(
        "import {{ Banner }} from \"./card\";\nexport function View() {{\n  return <section><Banner /><p title=\"{long_title}\"></p></section>;\n}}\n"
    );
    let same_line_import =
        "import { Banner } from \"./card\"; export const text = Banner();\n".to_string();
    for (user, expected) in [
        (&with_call, "mod"),
        (&same_line_import, "mod"),
        (&jsx_only, "mod_compat"),
    ] {
        let api = api_after(
            &[("src/card.tsx", card_old), ("src/use.tsx", user)],
            &[("src/card.tsx", card_new)],
        );
        assert_eq!(bucket(&api, "Banner"), expected, "{user}: {api:?}");
    }
}

/// メンバー参照 `api.fetchUser` は、参照ファイル自身に同名のラッパー関数があっても、その関数を
/// 指さない (受け手は削除元を `export *` で転送するバレル)。removed_dead に降格しない。
/// `` import(`./api/${name}.js`) `` の動的 import でも同じ。同じファイルの関数を識別子で
/// 呼ぶだけなら従来どおり removed_dead (対照)。
#[test]
fn member_reference_is_not_attributed_to_same_named_local_function() {
    let users_old = "export function fetchUser(id) {\n  return { id };\n}\n\nexport function listUsers() {\n  return [];\n}\n";
    let users_new = "export function listUsers() {\n  return [];\n}\n";
    for (service, expected) in [
        (
            "import * as api from \"./api/index.js\";\n\nexport function fetchUser(id) {\n  return api.fetchUser(id);\n}\n",
            "rm",
        ),
        (
            "export async function fetchUser(name, id) {\n  const mod = await import(`./api/${name}.js`);\n  return mod.fetchUser(id);\n}\n",
            "rm",
        ),
        (
            "function load(id) {\n  return { id };\n}\n\nexport function fetchUser(id) {\n  return load(id);\n}\n\nexport const first = fetchUser(1);\n",
            "rm_dead",
        ),
    ] {
        let api = api_after(
            &[
                ("src/api/users.js", users_old),
                ("src/api/index.js", "export * from \"./users.js\";\n"),
                ("src/service.js", service),
            ],
            &[("src/api/users.js", users_new)],
        );
        assert_eq!(bucket(&api, "fetchUser"), expected, "{service}: {api:?}");
    }
}

/// 名前空間経由の `a.helper()` は、同じファイルにある別モジュールからの同名 import
/// (`import { helper } from "./b.js"`) とは無関係。b.js の helper だけを呼ぶなら従来どおり
/// removed_dead (対照)。
#[test]
fn namespace_member_call_is_not_attributed_to_same_named_import() {
    let a_old = "export function helper() {\n  return \"a\";\n}\n\nexport function keep() {\n  return 1;\n}\n";
    let a_new = "export function keep() {\n  return 1;\n}\n";
    for (main, expected) in [
        (
            "import * as a from \"./a.js\";\nimport { helper } from \"./b.js\";\n\nexport function run() {\n  return [a.helper(), helper()];\n}\n",
            "rm",
        ),
        (
            "import { helper } from \"./b.js\";\n\nexport function run() {\n  return helper();\n}\n",
            "rm_dead",
        ),
    ] {
        let api = api_after(
            &[
                ("src/a.js", a_old),
                (
                    "src/b.js",
                    "export function helper() {\n  return \"b\";\n}\n",
                ),
                ("src/main.js", main),
            ],
            &[("src/a.js", a_new)],
        );
        assert_eq!(bucket(&api, "helper"), expected, "{main}: {api:?}");
    }
}

/// import 先のファイルに同名の「何らかの定義」(別クラスのメソッド) があるだけでは、import が
/// 残存定義を指すとは言えない (`export *` で削除元を転送している)。import 先が同名を通常の宣言で
/// export していれば従来どおり removed_dead (対照)。
#[test]
fn import_resolves_only_to_explicit_export_of_the_name() {
    let a_old =
        "export function helper() {\n  return 1;\n}\n\nexport function keep() {\n  return 2;\n}\n";
    let a_new = "export function keep() {\n  return 2;\n}\n";
    for (b, expected) in [
        (
            "export * from \"./a.js\";\n\nexport class Tool {\n  helper() {\n    return 0;\n  }\n}\n",
            "rm",
        ),
        ("export function helper() {\n  return 3;\n}\n", "rm_dead"),
    ] {
        let api = api_after(
            &[
                ("src/a.js", a_old),
                ("src/b.js", b),
                (
                    "src/main.js",
                    "import { helper } from \"./b.js\";\n\nexport const result = helper();\n",
                ),
            ],
            &[("src/a.js", a_new)],
        );
        assert_eq!(bucket(&api, "helper"), expected, "{b}: {api:?}");
    }
}

/// 外部パッケージから同名を import しているファイルでも、除外するのはその import の束縛の
/// 利用だけ。削除元の名前空間のメンバー `util.format` は数える。外部パッケージの束縛しか
/// 使っていなければ従来どおり removed_dead (対照)。
#[test]
fn external_import_excludes_only_its_own_binding() {
    let util_old = "export function format(n) {\n  return String(n);\n}\n\nexport function parse(s) {\n  return Number(s);\n}\n";
    let util_new = "export function parse(s) {\n  return Number(s);\n}\n";
    for (view, expected) in [
        (
            "import { format } from \"datefmt\";\nimport * as util from \"./util.js\";\n\nexport function show(d, n) {\n  return format(d) + util.format(n);\n}\n",
            "rm",
        ),
        (
            "import { format } from \"datefmt\";\n\nexport function show(d) {\n  return format(d);\n}\n",
            "rm_dead",
        ),
    ] {
        let api = api_after(
            &[
                (
                    "package.json",
                    "{\n  \"dependencies\": { \"datefmt\": \"^1.0.0\" }\n}\n",
                ),
                ("src/util.js", util_old),
                ("src/view.js", view),
            ],
            &[("src/util.js", util_new)],
        );
        assert_eq!(bucket(&api, "format"), expected, "{view}: {api:?}");
    }
}

const ERRORS_OLD: &str = "export interface RequestFailure {\n\terror: string;\n}\n\nexport function describe(message: string): string {\n\treturn message;\n}\n";
const ERRORS_NEW: &str =
    "export function describe(message: string): string {\n\treturn message;\n}\n";
const TRANSPORT: &str = "export class RequestFailure extends Error {\n\tstatus = 0;\n}\n";

/// tsconfig の paths の別名で import した同名のクラスは、削除した interface を指さない
/// (Issue #264)。別名で削除元のモジュール自身を import している、設定が別名を解かない
/// (バンドラーだけの別名かもしれない)、node16 で解決先が参照ファイルの形式に依る、のどれかなら
/// 従来どおり api.rm (対照)。
#[test]
fn tsconfig_paths_alias_import_is_attributed_to_its_target() {
    let alias = r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["./src/*"] } } }"#;
    for (tsconfig, spec, expected) in [
        (alias, "@/net/transport", "rm_dead"),
        (
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
            "@/net/transport",
            "rm_dead",
        ),
        (alias, "../net/transport", "rm_dead"),
        (alias, "@/server/errors", "rm"),
        (
            r#"{ "compilerOptions": { "baseUrl": "." } }"#,
            "@/net/transport",
            "rm",
        ),
        (
            r#"{ "compilerOptions": { "module": "Node16", "paths": { "@/*": ["./src/*"] } } }"#,
            "@/net/transport",
            "rm",
        ),
    ] {
        let user = format!(
            "import {{ RequestFailure }} from \"{spec}\";\n\nexport function isClientError(value: unknown): boolean {{\n\treturn value instanceof RequestFailure && value.status < 500;\n}}\n"
        );
        let api = api_after(
            &[
                ("tsconfig.json", tsconfig),
                ("src/net/transport.ts", TRANSPORT),
                ("src/server/errors.ts", ERRORS_OLD),
                ("src/app/use.ts", &user),
                (
                    "src/app/label.ts",
                    "import { describe } from \"../server/errors\";\n\nexport const label = describe(\"failed\");\n",
                ),
            ],
            &[("src/server/errors.ts", ERRORS_NEW)],
        );
        assert_eq!(
            bucket(&api, "RequestFailure"),
            expected,
            "{tsconfig} {spec}: {api:?}"
        );
    }
}

/// 最優先の paths のパターンの置換先が無いと、TypeScript は弱いパターンへ進まずに package.json の
/// `imports` で解く (ここでは削除元)。弱いパターンが生き残りのクラスに当たっても証明しない。
#[test]
fn alias_falling_back_to_package_imports_is_not_proven() {
    let api = api_after(
        &[
            (
                "tsconfig.json",
                r##"{ "compilerOptions": { "module": "ESNext", "moduleResolution": "bundler", "paths": { "#failure": ["./missing"], "*": ["./src/net/transport"] } } }"##,
            ),
            (
                "package.json",
                r##"{ "type": "module", "imports": { "#failure": "./src/server/errors.js" } }"##,
            ),
            ("src/net/transport.ts", TRANSPORT),
            ("src/server/errors.ts", ERRORS_OLD),
            (
                "src/app/use.ts",
                "import type { RequestFailure } from \"#failure\";\n\nexport type Failure = RequestFailure;\n",
            ),
        ],
        &[("src/server/errors.ts", ERRORS_NEW)],
    );
    assert_eq!(bucket(&api, "RequestFailure"), "rm", "{api:?}");
}

/// 依存の名前の import でも、tsconfig の baseUrl / paths がローカルのファイルへ差し替えていれば、
/// 外部パッケージとして除かない (削除したファイルを指す)。差し替えていなければ従来どおり外部
/// パッケージの束縛として除く (対照)。
#[test]
fn dependency_import_overridden_by_tsconfig_is_not_external() {
    let lodash_old =
        "export function debounce(): void {}\n\nexport function keep(): number {\n  return 1;\n}\n";
    let lodash_new = "export function keep(): number {\n  return 1;\n}\n";
    for (tsconfig, expected) in [
        (r#"{ "compilerOptions": { "baseUrl": "./src" } }"#, "rm"),
        (
            r#"{ "compilerOptions": { "paths": { "lodash": ["./src/lodash"] } } }"#,
            "rm",
        ),
        (r#"{ "compilerOptions": { "strict": true } }"#, "rm_dead"),
    ] {
        let api = api_after(
            &[
                (
                    "package.json",
                    "{\n  \"dependencies\": { \"lodash\": \"^4.0.0\" }\n}\n",
                ),
                ("tsconfig.json", tsconfig),
                ("src/lodash.ts", lodash_old),
                (
                    "src/app/use.ts",
                    "import { debounce } from \"lodash\";\n\nexport const run = (): void => debounce();\n",
                ),
            ],
            &[("src/lodash.ts", lodash_new)],
        );
        assert_eq!(bucket(&api, "debounce"), expected, "{tsconfig}: {api:?}");
    }
}

/// ワークスペースのパッケージの package.json にだけ書いた依存も、そのパッケージのファイルからの
/// import は外部パッケージの束縛として除く (Issue #280)。tsconfig がローカルのファイルへ差し替える
/// とき、祖先でないほかのパッケージからの import、`paths` の解決先のファイルを消すときは、従来どおり
/// 除かない。
#[test]
fn workspace_dependency_import_is_external_only_from_its_package() {
    let lodash_old =
        "export function debounce(): void {}\n\nexport function keep(): number {\n  return 1;\n}\n";
    let lodash_new = "export function keep(): number {\n  return 1;\n}\n";
    let user =
        "import { debounce } from \"lodash\";\n\nexport const run = (): void => debounce();\n";
    let strict = r#"{ "compilerOptions": { "strict": true } }"#;
    let paths = r#"{ "compilerOptions": { "paths": { "lodash": ["./src/lodash"] } } }"#;
    let files = |tsconfig: &'static str, user_path: &'static str| {
        vec![
            (
                "package.json",
                r#"{ "name": "mono", "private": true, "workspaces": ["packages/*"] }"#,
            ),
            (
                "packages/ui/package.json",
                r#"{ "name": "@example/ui", "dependencies": { "lodash": "^4.0.0" } }"#,
            ),
            (
                "packages/other/package.json",
                r#"{ "name": "@example/other" }"#,
            ),
            ("packages/ui/tsconfig.json", tsconfig),
            ("packages/ui/src/lodash.ts", lodash_old),
            (user_path, user),
        ]
    };
    for (tsconfig, user_path, expected) in [
        (strict, "packages/ui/src/app/use.ts", "rm_dead"),
        (
            r#"{ "compilerOptions": { "baseUrl": "./src" } }"#,
            "packages/ui/src/app/use.ts",
            "rm",
        ),
        (paths, "packages/ui/src/app/use.ts", "rm"),
        (strict, "packages/other/src/use.ts", "rm"),
    ] {
        let api = api_after(
            &files(tsconfig, user_path),
            &[("packages/ui/src/lodash.ts", lodash_new)],
        );
        assert_eq!(
            bucket(&api, "debounce"),
            expected,
            "{tsconfig} {user_path}: {api:?}"
        );
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &files(paths, "packages/ui/src/app/use.ts"), "base");
    fs::remove_file(repo.join("packages/ui/src/lodash.ts")).expect("remove");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(bucket(&api, "debounce"), "rm", "{api:?}");
}

/// 大文字小文字を区別しないファイルシステムでは、綴りの違う置換先 (`./SRC/SERVER/errors`) も
/// 削除したファイルを指す。削除したファイルと同じ名前の JS のファイルが残っていても証明しない。
#[test]
fn alias_spelled_in_other_case_still_points_to_the_removed_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "@failure": ["./SRC/SERVER/errors"] } } }"#,
            ),
            (
                "src/server/errors.ts",
                "export function RequestFailure(): void {}\n",
            ),
            ("src/server/errors.js", "export class RequestFailure {}\n"),
            (
                "src/app/use.ts",
                "import { RequestFailure } from \"@failure\";\n\nRequestFailure();\n",
            ),
        ],
        "base",
    );
    fs::remove_file(repo.join("src/server/errors.ts")).expect("remove");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(bucket(&api, "RequestFailure"), "rm", "{api:?}");
}

/// default export / 別名 export で公開していた関数を削除すると、利用者は公開名で import して
/// いるので宣言名の参照は 0 件になる。参照 0 件を根拠に removed_dead へ落とさない。宣言名の
/// まま公開していた未使用の関数の削除は従来どおり removed_dead (対照)。
#[test]
fn removed_default_or_aliased_export_stays_blocking() {
    for (old, user, expected) in [
        (
            "export default function formatPrice(value) {\n  return String(value);\n}\n\nexport function formatTotal(values) {\n  return values.join(\",\");\n}\n",
            "import fmt from \"./format.js\";\nexport const label = fmt(10);\n",
            "rm",
        ),
        (
            "function formatPrice(value) {\n  return String(value);\n}\nexport { formatPrice as price };\n\nexport function formatTotal(values) {\n  return values.join(\",\");\n}\n",
            "import { price } from \"./format.js\";\nexport const label = price(10);\n",
            "rm",
        ),
        (
            "export function formatPrice(value) {\n  return String(value);\n}\n\nexport function formatTotal(values) {\n  return values.join(\",\");\n}\n",
            "import { formatTotal } from \"./format.js\";\nexport const label = formatTotal([1]);\n",
            "rm_dead",
        ),
    ] {
        let api = api_after(
            &[("src/format.js", old), ("src/use.js", user)],
            &[(
                "src/format.js",
                "export function formatTotal(values) {\n  return values.join(\",\");\n}\n",
            )],
        );
        assert_eq!(bucket(&api, "formatPrice"), expected, "{old}: {api:?}");
    }
}
