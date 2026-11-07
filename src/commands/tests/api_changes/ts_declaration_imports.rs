//! TypeScript の相対 import が宣言ファイルに解かれる削除 (Issue #272)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

const TSCONFIG: &str = "{ \"compilerOptions\": { \"allowJs\": true } }\n";
const DECLARATION: &str =
    "export interface RequestFailure {\n  error: string;\n}\nexport declare const keep: number;\n";
const DECLARATION_AFTER: &str = "export declare const keep: number;\n";
const IMPLEMENTATION: &str = "export class RequestFailure {}\nexport const keep = 1;\n";

/// `declaration` から `RequestFailure` を消したときの区分 (`rm` / `rm_dead` / `none`)。
/// `files` はコミットするほかのファイル。
fn removal_bucket(declaration: &str, files: &[(&str, &str)]) -> &'static str {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut committed = vec![("tsconfig.json", TSCONFIG), (declaration, DECLARATION)];
    committed.extend_from_slice(files);
    git_commit_files(repo, &committed, "base");
    fs::write(repo.join(declaration), DECLARATION_AFTER).expect("write");
    let api = detect_api_changes_from_worktree(repo);
    let named = |symbols: &[crate::models::review::ApiSymbol]| {
        symbols.iter().any(|symbol| symbol.name == "RequestFailure")
    };
    if named(&api.removed) {
        "rm"
    } else if named(&api.removed_dead) {
        "rm_dead"
    } else {
        "none"
    }
}

/// TypeScript は相対 import をまず宣言ファイルに解くので、同名の JS の実装が隣に残っていても、
/// 宣言ファイルから消した export を指す型の参照は残る (Issue #272 の再現)。
#[test]
fn ts_declaration_removal_with_sibling_implementation_stays_blocking() {
    for (declaration, implementation, spec) in [
        (
            "src/server/errors.d.ts",
            "src/server/errors.js",
            "../server/errors",
        ),
        (
            "src/server/errors.d.ts",
            "src/server/errors.js",
            "../server/errors.js",
        ),
        (
            "src/server/errors.d.mts",
            "src/server/errors.mjs",
            "../server/errors.mjs",
        ),
        (
            "src/server/errors.d.cts",
            "src/server/errors.cjs",
            "../server/errors.cjs",
        ),
        (
            "src/server/errors/index.d.ts",
            "src/server/errors/index.js",
            "../server/errors",
        ),
    ] {
        let user = format!(
            "import type {{ RequestFailure }} from \"{spec}\";\n\nexport type Failure = RequestFailure;\n"
        );
        assert_eq!(
            removal_bucket(
                declaration,
                &[(implementation, IMPLEMENTATION), ("src/app/use.ts", &user)]
            ),
            "rm",
            "{declaration} via {spec}"
        );
    }
}

/// 対照: 参照が削除元でない別のファイルの同名の export に解かれるなら、従来どおり removed_dead。
#[test]
fn ts_declaration_removal_resolved_elsewhere_is_dead() {
    assert_eq!(
        removal_bucket(
            "src/server/errors.d.ts",
            &[
                ("src/server/errors.js", IMPLEMENTATION),
                (
                    "src/shared/errors.ts",
                    "export interface RequestFailure {\n  error: string;\n}\n"
                ),
                (
                    "src/app/use.ts",
                    "import type { RequestFailure } from \"../shared/errors\";\n\nexport type Failure = RequestFailure;\n"
                ),
            ]
        ),
        "rm_dead"
    );
}

/// 大文字小文字を区別しないファイルシステムでは、綴りの違う指定子も削除元を指すので、同じ名前の
/// ローカルの宣言に束縛される参照でも証明しない。
#[test]
fn ts_declaration_removal_with_case_variant_dependency_stays_blocking() {
    assert_eq!(
        removal_bucket(
            "src/server/errors.d.ts",
            &[
                ("src/server/errors.js", IMPLEMENTATION),
                (
                    "src/app/local.ts",
                    "import { keep } from \"../Server/Errors\";\n\ninterface RequestFailure {\n  local: boolean;\n}\n\nexport type Failure = RequestFailure;\nexport const value = keep;\n"
                ),
            ]
        ),
        "rm"
    );
}

/// 候補で網羅できない解決 (設定の `moduleSuffixes`・`rootDirs`、ディレクトリの package.json の
/// `types`) と、node10 の付け足した名前 (`errors.js.d.ts`) の宣言ファイルも、証明せずに blocking に
/// 残す。
#[test]
fn ts_declaration_removal_with_unlisted_resolution_stays_blocking() {
    let user = |spec: &str| {
        format!(
            "import type {{ RequestFailure }} from \"{spec}\";\n\nexport type Failure = RequestFailure;\n"
        )
    };
    let suffixed = user("../server/errors");
    let with_suffixes = [
        (
            "tsconfig.json",
            "{ \"compilerOptions\": { \"allowJs\": true, \"moduleSuffixes\": [\".native\", \"\"] } }\n",
        ),
        ("src/server/errors.js", IMPLEMENTATION),
        ("src/app/use.ts", suffixed.as_str()),
    ];
    assert_eq!(
        removal_bucket("src/server/errors.native.d.ts", &with_suffixes),
        "rm"
    );
    let directory = user("../server/errors");
    assert_eq!(
        removal_bucket(
            "src/server/errors/api.d.ts",
            &[
                (
                    "src/server/errors/package.json",
                    "{ \"types\": \"./api.d.ts\" }\n"
                ),
                ("src/server/errors/index.js", IMPLEMENTATION),
                ("src/app/use.ts", &directory),
            ]
        ),
        "rm"
    );
    let legacy = user("../server/errors.js");
    assert_eq!(
        removal_bucket(
            "src/server/errors.js.d.ts",
            &[
                ("src/server/errors.js", IMPLEMENTATION),
                ("src/app/use.ts", &legacy),
            ]
        ),
        "rm"
    );
}

/// 対照: 残る宣言ファイルの `export declare class` は、そのファイルの宣言として証明に数える。
#[test]
fn ts_declaration_removal_resolved_to_ambient_declaration_is_dead() {
    assert_eq!(
        removal_bucket(
            "src/server/unused.d.ts",
            &[
                ("src/shared.js", "export class RequestFailure {}\n"),
                (
                    "src/shared.d.ts",
                    "export declare class RequestFailure {}\n"
                ),
                (
                    "src/app/use.ts",
                    "import type { RequestFailure } from \"../shared.js\";\n\nexport type Failure = RequestFailure;\n"
                ),
            ]
        ),
        "rm_dead"
    );
}

/// 設定の `extends` の先は、前の設定を解釈できなくても辿り、相対の探索を変える設定を見つける。
/// TypeScript の付け足した名前 (`errors.ts.d.ts`) も候補に入れる。
#[test]
fn ts_declaration_removal_with_partially_readable_configs_stays_blocking() {
    let user = "import type { RequestFailure } from \"./errors\";\n\nexport type Failure = RequestFailure;\n";
    assert_eq!(
        removal_bucket(
            "src/errors.native.d.ts",
            &[
                (
                    "tsconfig.json",
                    "{ \"extends\": [\"./config/first.json\", \"./config/native.json\"], \"compilerOptions\": { \"allowJs\": true, \"moduleResolution\": \"node10\" } }\n",
                ),
                (
                    "config/first.json",
                    "{ \"compilerOptions\": { \"baseUrl\": null } }\n"
                ),
                (
                    "config/native.json",
                    "{ \"compilerOptions\": { \"moduleSuffixes\": [\".native\", \"\"] } }\n"
                ),
                ("src/errors.js", IMPLEMENTATION),
                ("src/use.ts", user),
            ]
        ),
        "rm"
    );
    assert_eq!(
        removal_bucket(
            "src/errors.ts.d.ts",
            &[
                ("src/errors.js", IMPLEMENTATION),
                (
                    "src/use.ts",
                    "import type { RequestFailure } from \"./errors.ts\";\n\nexport type Failure = RequestFailure;\n"
                ),
            ]
        ),
        "rm"
    );
}

/// 対照: 1 つ以下の `rootDirs` と、`null` の `moduleSuffixes` は相対の探索を変えないので、従来どおり
/// 証明する。
#[test]
fn ts_declaration_removal_with_harmless_resolution_options_is_dead() {
    for options in [
        "\"rootDirs\": [\"src\"]",
        "\"rootDirs\": []",
        "\"moduleSuffixes\": null",
        "\"moduleSuffixes\": [\"\"]",
    ] {
        let config = format!("{{ \"compilerOptions\": {{ \"allowJs\": true, {options} }} }}\n");
        assert_eq!(
            removal_bucket(
                "types/unused.d.ts",
                &[
                    ("tsconfig.json", &config),
                    (
                        "src/shared.ts",
                        "export interface RequestFailure {\n  local: boolean;\n}\n"
                    ),
                    (
                        "src/use.ts",
                        "import type { RequestFailure } from \"./shared\";\n\nexport type Failure = RequestFailure;\n"
                    ),
                ]
            ),
            "rm_dead",
            "{options}"
        );
    }
}
