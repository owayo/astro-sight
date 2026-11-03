use super::support::{TestRepo, cargo_bin};

#[test]
fn trailing_optional_removal_rejects_dynamic_execution_in_origin() {
    for execution in [
        "spacing(); eval('spacing(2)');",
        "spacing(); (0, eval)('spacing(2)');",
        "spacing(); new Function('spacing(2)')();",
        "spacing(); const evaluate = eval; evaluate('spacing(2)');",
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            format!(
                "export function spacing(scale = 1): number {{ return scale; }}\n{execution}\n"
            ),
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            format!("export function spacing(): number {{ return 1; }}\n{execution}\n"),
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{execution}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{json}");
    }
}

#[test]
fn trailing_optional_addition_keeps_existing_policy() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "spacing.ts",
        "export function spacing(): number { return 1; }\n",
    );
    repo.write(
        "use.ts",
        "import { spacing } from './spacing';\nspacing();\n",
    );
    repo.commit_all("initial");
    repo.write(
        "spacing.ts",
        "export function spacing(scale = 1): number { return scale; }\n",
    );
    let (output, json) = optional_removal_hook(&repo);
    assert!(output.status.success(), "{json}");
    assert_eq!(
        json["api"]["mod_compat"][0]["reason"], "trailing_optional_params",
        "{json}"
    );
}

fn optional_removal_hook(repo: &TestRepo) -> (std::process::Output, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let json = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "invalid hook output: {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, json)
}

#[test]
fn trailing_optional_removal_spacing_is_repository_local_call_compatible() {
    for extension in ["ts", "tsx"] {
        let repo = TestRepo::new();
        repo.init_git();
        let file = format!("spacing.{extension}");
        repo.write(
            &file,
            "export function spacing(scale = 1): { gap: number } { return { gap: 12 * scale }; }\n",
        );
        repo.write(format!("use.{extension}"), "import { spacing } from './spacing';\nexport function gapOf(): number { return spacing().gap; }\n");
        repo.commit_all("initial");
        repo.write(
            &file,
            "export function spacing(): { gap: number } { return { gap: 12 }; }\n",
        );
        let api = repo.run_json("review", &["--git"]);
        assert_eq!(
            api["api_changes"]["compatible_modified"][0]["reason"],
            "trailing_optional_params_removed",
            "{api}"
        );
        let (output, json) = optional_removal_hook(&repo);
        assert!(output.status.success(), "{json}");
        assert_eq!(
            json["api"]["mod_compat"][0]["reason"], "trailing_optional_params_removed",
            "{json}"
        );
        assert!(json["impacts"].is_null(), "{json}");
        assert!(
            json["blocking_categories"].as_array().unwrap().is_empty(),
            "{json}"
        );
        assert!(
            json["impact_info"].as_array().unwrap().iter().any(|group| {
                group["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|reference| reference["ln"] == 1)
            }),
            "actual call must remain informational: {json}"
        );
    }
}

#[test]
fn trailing_optional_removal_keeps_remaining_parameters_and_comment_arguments() {
    for extension in ["ts", "tsx"] {
        for optional in ["scale = 1", "scale?: number"] {
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("spacing.{extension}");
            repo.write(&file, format!("export function spacing(size: number, {optional}): number {{ return size; }}\nfunction ownCall(): number {{ return spacing(2); }}\n"));
            repo.write(format!("use.{extension}"), "import { spacing } from './spacing';\nexport function read(): number { return spacing(/* ordinary comment */ 2, /* tail */); }\n");
            repo.commit_all("initial");
            repo.write(&file, "export function spacing(size: number): number { return size; }\nfunction ownCall(): number { return spacing(2); }\n");
            let (output, json) = optional_removal_hook(&repo);
            assert!(output.status.success(), "{json}");
            assert_eq!(
                json["api"]["mod_compat"][0]["reason"], "trailing_optional_params_removed",
                "{json}"
            );
        }
    }
}

#[test]
fn trailing_optional_removal_rejects_unproven_uses() {
    for extension in ["ts", "tsx"] {
        for usage in [
            "spacing(2);",
            "spac\\u0069ng(2); spacing();",
            "spacing(undefined);",
            "spacing(...values);",
            "const alias = spacing; alias();",
            "consume(spacing);",
            "type Args = Parameters<typeof spacing>; spacing();",
            "const registry = { spacing }; spacing();",
            "spacing.call(null);",
            "new spacing();",
            "spacing?.();",
            "(spacing)();",
            "spacing<string>();",
            "spacing(); function shadow(spacing: () => number) { return spacing(); }",
            "const view = <spacing />; spacing();",
        ] {
            if extension == "ts" && usage.contains("<spacing") {
                continue;
            }
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("spacing.{extension}");
            repo.write(
                &file,
                "export function spacing(scale = 1): number { return scale; }\n",
            );
            repo.write(
                format!("use.{extension}"),
                format!("import {{ spacing }} from './spacing';\n{usage}\n"),
            );
            repo.commit_all("initial");
            repo.write(&file, "export function spacing(): number { return 1; }\n");
            let (output, json) = optional_removal_hook(&repo);
            assert_eq!(output.status.code(), Some(1), "{usage}: {json}");
            assert!(json["api"]["mod_compat"].is_null(), "{usage}: {json}");
        }
    }
}

#[test]
fn trailing_optional_removal_rejects_contract_changes() {
    for (before, after) in [
        ("(scale: number): number", "(): number"),
        ("(scale = 1)", "()"),
        ("(scale = 1): number", "(): string"),
        (
            "(size: number, scale = 1): number",
            "(size: string): number",
        ),
        ("(size: number, scale = 1): 'a b'", "(size: number): 'a  b'"),
        (
            "(label: 'a b', scale = 1): number",
            "(label: 'a  b'): number",
        ),
        ("<T>(scale = 1): number", "<T>(): number"),
        (
            "(this: object, scale = 1): number",
            "(this: object): number",
        ),
        ("(scale = 1, ...values: number[]): number", "(): number"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            format!("export function spacing{before} {{ return 1; }}\n"),
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            format!("export function spacing{after} {{ return 1; }}\n"),
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{before} -> {after}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{json}");
    }
}

#[test]
fn trailing_optional_removal_rejects_hidden_module_transport() {
    for transport in [
        "eval('spacing(2)');",
        "ev\\u0061l('spacing(2)');",
        "requ\\u0069re('./spacing');",
        "Function('spacing(2)')();",
        "new Function('spacing(2)')();",
        "const evaluate = eval; evaluate('spacing(2)');",
        "const construct = Function; construct('spacing(2)')();",
        "globalThis['eval']('spacing(2)');",
        "export * from './spacing';",
        "export { other as forwarded } from './spacing';",
        "import * as helpers from './spacing';",
        "import { spacing as local } from './spacing';",
        "import { 'spacing' as local } from './spacing';",
        "import type { spacing } from './spacing';",
        "import('./spacing');",
        "import(`./spacing`);",
        "import(path);",
        "require('./spacing');",
        "require(path);",
        "const load = require; load('./spacing');",
        "consume(require);",
        "import.meta.glob('./*.ts');",
        "type Exported = typeof import('./spacing');",
        "declare module './spacing' { export function extra(): void; }",
        "import { spacing as local } from '@paths/spacing';",
        "import { spacing as local } from './spac\\u0069ng';",
        "import local = require('./spacing');",
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            "export function spacing(scale = 1): number { return scale; }\n",
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.write("transport.ts", transport);
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            "export function spacing(): number { return 1; }\n",
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{transport}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{transport}: {json}");
    }
}

#[test]
fn trailing_optional_removal_tolerates_unrelated_named_imports() {
    let removal_repo = |other: &str| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            "export function spacing(scale = 1): number { return scale; }\n",
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.write("other.ts", other);
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            "export function spacing(): number { return 1; }\n",
        );
        repo
    };
    // 解決できない指定子でも、対象と別の名前だけの named import は対象を運ばない。
    for other in [
        "import { useState } from 'react';\nexport const state = useState;\n",
        "import { useState as useLocal } from 'react';\nexport const state = useLocal;\n",
        "import type { FC } from 'react';\nexport type View = FC;\n",
        "import { type FC, useMemo } from 'react';\nexport type View = FC;\nexport const memo = useMemo;\n",
    ] {
        let (output, json) = optional_removal_hook(&removal_repo(other));
        assert!(output.status.success(), "{other}: {json}");
        assert_eq!(
            json["api"]["mod_compat"][0]["reason"], "trailing_optional_params_removed",
            "{other}: {json}"
        );
    }
    // 対象と同じ名前・default・namespace・文字列の名前を含む文は、従来どおり解決を求める。
    for other in [
        "import { spacing as local } from 'pkg';\nlocal();\n",
        "import { other as spacing } from 'pkg';\nspacing();\n",
        "import { 'spacing' as local } from 'pkg';\nlocal();\n",
        "import { default as local } from 'pkg';\nlocal();\n",
        "import React, { useState } from 'react';\nexport const state = [React, useState];\n",
        "import * as React from 'react';\nexport const state = React;\n",
        "import React from 'react';\nexport const state = React;\n",
        "import 'side-effect';\n",
        "export { useState } from 'react';\n",
    ] {
        let (output, json) = optional_removal_hook(&removal_repo(other));
        assert_eq!(output.status.code(), Some(1), "{other}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{other}: {json}");
    }
}

#[test]
fn trailing_optional_removal_rejects_package_bridges_outside_the_registry() {
    // node_modules の中のパッケージが、関数を別名で再 export して、消した引数を渡す。
    let bridge_repo = |extra: (&str, &str)| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            "export function spacing(scale = 1): number { return scale; }\n",
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.write(".gitignore", "node_modules/\n");
        repo.create_dir_all("node_modules/bridge");
        repo.write(
            "node_modules/bridge/index.d.ts",
            "export { spacing as space } from '../../spacing';\n",
        );
        repo.write("other.ts", "import { space } from 'bridge';\nspace(2);\n");
        if let Some(parent) = std::path::Path::new(extra.0).parent() {
            repo.create_dir_all(parent);
        }
        repo.write(extra.0, extra.1);
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            "export function spacing(): number { return 1; }\n",
        );
        repo
    };
    for extra in [
        ("packages/bridge/package.json", "{ \"name\": \"bridge\" }\n"),
        (
            "package.json",
            "{ \"name\": \"app\", \"dependencies\": { \"bridge\": \"file:./vendor/bridge\" } }\n",
        ),
    ] {
        let (output, json) = optional_removal_hook(&bridge_repo(extra));
        assert_eq!(output.status.code(), Some(1), "{}: {json}", extra.1);
        assert!(json["api"]["mod_compat"].is_null(), "{}: {json}", extra.1);
    }
    // 対照: registry の依存からの無関係な named import は受け入れる。
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "spacing.ts",
        "export function spacing(scale = 1): number { return scale; }\n",
    );
    repo.write(
        "use.ts",
        "import { spacing } from './spacing';\nimport { useState } from 'react';\nspacing();\nexport const state = useState;\n",
    );
    repo.write(
        "package.json",
        "{ \"name\": \"app\", \"dependencies\": { \"react\": \"^18.2.0\" } }\n",
    );
    repo.commit_all("initial");
    repo.write(
        "spacing.ts",
        "export function spacing(): number { return 1; }\n",
    );
    let (output, json) = optional_removal_hook(&repo);
    assert!(output.status.success(), "{json}");
    assert_eq!(
        json["api"]["mod_compat"][0]["reason"], "trailing_optional_params_removed",
        "{json}"
    );
}

#[test]
fn trailing_optional_removal_rejects_incomplete_inventory() {
    for (path, source) in [
        ("spacing.tsx", "export const other = 1;"),
        ("broken.ts", "const broken = ;"),
        ("component.vue", "<script>load('./spacing')</script>"),
        ("foreign.py", "spacing()"),
        (
            "generated.ts",
            "// @generated\nimport * as helpers from './spacing';",
        ),
        (
            "other.ts",
            "function spacing(): number { return 1; }\nspacing();",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "spacing.ts",
            "export function spacing(scale = 1): number { return scale; }\n",
        );
        repo.write(
            "use.ts",
            "import { spacing } from './spacing';\nspacing();\n",
        );
        repo.write(path, source);
        repo.commit_all("initial");
        repo.write(
            "spacing.ts",
            "export function spacing(): number { return 1; }\n",
        );
        let (output, json) = optional_removal_hook(&repo);
        assert_eq!(output.status.code(), Some(1), "{path}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{path}: {json}");
    }
}

#[test]
fn trailing_optional_removal_preserves_other_blocking_origins() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("spacing.ts", "export function spacing(scale = 1): number { return scale; }\nexport function other(): number { return 1; }\n");
    repo.write(
        "second.ts",
        "export function alternate(): number { return 1; }\n",
    );
    repo.write("use.ts", "import { spacing, other } from './spacing';\nimport { alternate } from './second';\nspacing(); other(); alternate();\n");
    repo.commit_all("initial");
    repo.write("spacing.ts", "export function spacing(): number { return 1; }\nexport function other(value: number): number { return value; }\n");
    repo.write(
        "second.ts",
        "export function alternate(value: number): number { return value; }\n",
    );
    let (output, json) = optional_removal_hook(&repo);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert_eq!(json["api"]["mod_compat"][0]["n"], "spacing", "{json}");
    let groups = json["impacts"].as_array().unwrap();
    assert!(
        groups.iter().any(|group| group["src"] == "spacing.ts"
            && group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol == "other")),
        "{json}"
    );
    assert!(
        groups.iter().any(|group| group["src"] == "second.ts"
            && group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol == "alternate")),
        "{json}"
    );
    assert!(
        groups.iter().all(|group| !group["syms"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol == "spacing")),
        "{json}"
    );
}

/// `spacing` の末尾の省略可能引数を消す。`files` も最初のコミットに入れる (ignore したものは
/// 作業ツリーにだけ残る)。
fn spacing_removal_repo(files: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "spacing.ts",
        "export function spacing(scale = 1): number { return scale; }\n",
    );
    repo.write(
        "use.ts",
        "import { spacing } from './spacing';\nspacing();\n",
    );
    for (path, source) in files {
        if let Some(parent) = std::path::Path::new(path).parent() {
            repo.create_dir_all(parent);
        }
        repo.write(path, source);
    }
    repo.commit_all("initial");
    repo.write(
        "spacing.ts",
        "export function spacing(): number { return 1; }\n",
    );
    repo
}

fn assert_removal(repo: &TestRepo, compatible: bool, context: &str) {
    let (output, json) = optional_removal_hook(repo);
    if compatible {
        assert!(output.status.success(), "{context}: {json}");
        assert_eq!(
            json["api"]["mod_compat"][0]["reason"], "trailing_optional_params_removed",
            "{context}: {json}"
        );
    } else {
        assert_eq!(output.status.code(), Some(1), "{context}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{context}: {json}");
    }
}

const REACT_IMPORT: (&str, &str) = (
    "state.ts",
    "import { useState } from 'react';\nexport const state = useState;\n",
);

#[test]
fn trailing_optional_removal_inventories_ignored_and_hidden_files() {
    // ignore したディレクトリと隠しディレクトリの中の呼び出しも在庫に入れる。
    for (ignore, path) in [
        ((".ignore", "generated/\n"), "generated/use.ts"),
        ((".gitignore", "generated/\n"), "generated/use.ts"),
        ((".gitignore", "\n"), ".scripts/use.ts"),
    ] {
        for (call, compatible) in [("spacing(2);", false), ("spacing();", true)] {
            let source = format!("import {{ spacing }} from '../spacing';\n{call}\n");
            let repo = spacing_removal_repo(&[ignore, (path, &source)]);
            assert_removal(&repo, compatible, &format!("{ignore:?} {path}: {call}"));
        }
    }
}

#[test]
fn trailing_optional_removal_checks_every_package_manifest() {
    // node_modules の bridge が関数を別名で再 export して、消した引数を渡す。other.ts に近い
    // app/package.json は bridge を指定しない。
    let bridge = [
        (".gitignore", "node_modules/\n"),
        (
            "node_modules/bridge/index.d.ts",
            "export { spacing as space } from '../../spacing';\n",
        ),
        (
            "app/other.ts",
            "import { space } from 'bridge';\nspace(2);\n",
        ),
        ("app/package.json", "{ \"name\": \"app\" }\n"),
    ];
    for manifest in [
        "{ \"name\": \"root\", \"dependencies\": { \"bridge\": \"file:../bridge\" } }\n",
        "{ \"name\": \"root\", \"dependencies\": { \"bridge\": \"^1.0.0\" }, \"overrides\": { \"bridge\": \"file:../bridge\" } }\n",
        "{ \"name\": \"root\", \"pnpm\": { \"overrides\": { \"parent>bridge\": \"link:../bridge\" } } }\n",
    ] {
        let mut files = bridge.to_vec();
        files.push(("package.json", manifest));
        assert_removal(&spacing_removal_repo(&files), false, manifest);
    }
    // 対照: 祖先の package.json の registry の依存 (react) からの named import は受け入れる。
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        ("app/package.json", "{ \"name\": \"app\" }\n"),
        (
            "package.json",
            "{ \"name\": \"root\", \"dependencies\": { \"react\": \"^18.2.0\" } }\n",
        ),
    ]);
    assert_removal(&repo, true, "registry dependency of an ancestor");
}

#[cfg(unix)]
#[test]
fn trailing_optional_removal_does_not_relax_imports_without_the_audit() {
    // ディレクトリの symlink の先は監査できないので、registry の named import の緩和を使わない。
    let repo = spacing_removal_repo(&[REACT_IMPORT, ("assets/logo.svg", "<svg />\n")]);
    std::os::unix::fs::symlink(repo.path("assets"), repo.path("mirror")).unwrap();
    assert_removal(&repo, false, "directory symlink");
    // 対照: symlink が無ければ、同じ import を受け入れる。
    let repo = spacing_removal_repo(&[REACT_IMPORT, ("assets/logo.svg", "<svg />\n")]);
    assert_removal(&repo, true, "no symlink");
}

/// 名前を含まない、ignore した生成物と他言語のファイル。在庫の上限 (512 ファイル・8 MiB) を超える
/// 数と量のビルド出力を含む。
fn unrelated_ignored_files() -> Vec<(String, String)> {
    let mut files = vec![
        (
            ".gitignore".to_owned(),
            "dist/\n.venv/\ntarget/\n".to_owned(),
        ),
        (
            ".husky/pre-commit".to_owned(),
            "#!/bin/sh\nnpx lint-staged\n".to_owned(),
        ),
        (
            ".venv/lib/python3.12/site-packages/tool/__init__.py".to_owned(),
            "def run():\n    return 1\n".to_owned(),
        ),
        (
            "target/debug/build/out/bindings.rs".to_owned(),
            "pub fn run() {}\n".to_owned(),
        ),
        (
            "target/doc/search-index.js".to_owned(),
            "var searchIndex = new Map();\n".to_owned(),
        ),
        (
            "dist/vendor.js".to_owned(),
            format!("export const blob = \"{}\";\n", "a".repeat(9_000_000)),
        ),
    ];
    files.extend((0..520).map(|index| {
        (
            format!("dist/chunks/chunk-{index}.js"),
            format!("export const value{index} = {index};\n"),
        )
    }));
    files
}

#[test]
fn trailing_optional_removal_tolerates_unrelated_ignored_files() {
    // shell の hook・.venv・target・名前の現れない大きなビルド出力は、在庫に入れない (この PR の
    // 前と同じ結果)。
    let extra = unrelated_ignored_files();
    let mut files: Vec<(&str, &str)> = extra
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    files.push(REACT_IMPORT);
    assert_removal(
        &spacing_removal_repo(&files),
        true,
        "unrelated ignored files",
    );
}

#[test]
fn trailing_optional_removal_rejects_ignored_outputs_that_mention_the_name() {
    // 名前の現れる ignore したビルド出力は在庫に加え、同じ規則で証明する。
    let large = format!("spacing;\n{}\n", "/* padding */".repeat(700_000));
    for (path, source) in [
        (
            "dist/use.js",
            "import { spacing } from '../spacing';\nspacing(2);\n",
        ),
        (
            "dist/min.js",
            "import{spacing as s}from\"../spacing.js\";s(2);\n",
        ),
        // 名前が現れて在庫に入るファイルには、在庫の上限 (8 MiB) を掛ける。
        ("dist/big.js", large.as_str()),
    ] {
        let repo = spacing_removal_repo(&[(".gitignore", "dist/\n"), (path, source)]);
        assert_removal(&repo, false, path);
    }
}

#[test]
fn trailing_optional_removal_tolerates_pattern_ambient_modules() {
    // `*` を含む ambient module は、ファイルに解決できない指定子にだけ当たる。
    let repo = spacing_removal_repo(&[(
        "types/assets.d.ts",
        "declare module '*.svg' {\n\tconst url: string;\n\texport default url;\n}\n",
    )]);
    assert_removal(&repo, true, "pattern ambient module");
    // 対照: パターンでない名前の宣言は、解決できないので従来どおり在庫を閉じない。
    let repo = spacing_removal_repo(&[(
        "types/assets.d.ts",
        "declare module 'assets' {\n\texport function spacing(scale: number): number;\n}\n",
    )]);
    assert_removal(&repo, false, "named ambient module");
}

#[test]
fn trailing_optional_removal_needs_the_audit_for_the_relaxation() {
    // 監査の印 (`declare`) が現れて解析する ignore したファイルが解析の上限を超えると、監査を
    // 作らず、registry の named import の緩和も使わない。
    let padding = "// padding\n".repeat(900_000);
    let marked = format!("declare const blob: string;\n{padding}");
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        (".gitignore", "generated/\n"),
        ("generated/types.d.ts", &marked),
    ]);
    assert_removal(&repo, false, "matched source over the parse limit");
    // 対照: 印の無い同じ大きさのファイルは解析しないので、緩和を使える。
    let plain = format!("export const blob = 1;\n{padding}");
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        (".gitignore", "generated/\n"),
        ("generated/types.d.ts", &plain),
    ]);
    assert_removal(&repo, true, "unmarked source over the parse limit");
}

#[test]
fn trailing_optional_removal_resolves_npm_override_references() {
    // overrides の `$donor` は、donor の指定 (registry の外) を参照する。bridge は関数を別名で
    // 再 export して、消した引数を渡す。
    let repo = spacing_removal_repo(&[
        (".gitignore", "node_modules/\n"),
        (
            "package.json",
            "{ \"name\": \"app\", \"dependencies\": { \"donor\": \"file:../local-bridge\", \"bridge\": \"^1.0.0\" }, \"overrides\": { \"bridge\": \"$donor\" } }\n",
        ),
        (
            "node_modules/bridge/index.d.ts",
            "export { spacing as space } from '../../spacing';\n",
        ),
        ("other.ts", "import { space } from 'bridge';\nspace(2);\n"),
    ]);
    assert_removal(&repo, false, "override referencing a local donor");
    // 対照: registry の指定を参照する `$` は、registry の依存のまま。
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        (
            "package.json",
            "{ \"name\": \"app\", \"dependencies\": { \"react\": \"^18.2.0\" }, \"overrides\": { \"react\": \"$react\" } }\n",
        ),
    ]);
    assert_removal(&repo, true, "override referencing a registry dependency");
}

#[test]
fn trailing_optional_removal_selects_ignored_declarations_by_dependencies() {
    // utils.ts の spacing を、ignore した宣言ファイルが namespace import の型から使う (名前も
    // 宣言のファイルの名前も、宣言ファイルの語としては数えない)。
    let repo_with = |declaration: &str| {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "utils.ts",
            "export function spacing(scale = 1): number { return scale; }\n",
        );
        repo.write("use.ts", "import { spacing } from './utils';\nspacing();\n");
        repo.write(".gitignore", "generated/\n");
        repo.create_dir_all("generated");
        repo.write("generated/use.d.ts", declaration);
        repo.commit_all("initial");
        repo.write(
            "utils.ts",
            "export function spacing(): number { return 1; }\n",
        );
        repo
    };
    assert_removal(
        &repo_with(
            "import type * as u from '../utils';\nexport type P = Parameters<(typeof u)[keyof typeof u]>;\n",
        ),
        false,
        "declaration importing the target",
    );
    // 対照: 宣言のファイルに依存しない宣言ファイルは在庫に入れない。
    assert_removal(
        &repo_with("export type P = [number];\n"),
        true,
        "declaration without a dependency",
    );
}

#[test]
fn trailing_optional_removal_reads_package_manager_configs() {
    // pnpm-workspace.yaml の overrides がローカルの依存に差し替えると、package.json の registry の
    // 指定があっても registry とは決めない。bridge は関数を別名で再 export して、消した引数を渡す。
    let bridge = |config: (&str, &str)| {
        spacing_removal_repo(&[
            (".gitignore", "node_modules/\n"),
            (
                "package.json",
                "{ \"name\": \"app\", \"dependencies\": { \"bridge\": \"^1.0.0\" } }\n",
            ),
            config,
            (
                "node_modules/bridge/index.d.ts",
                "export { spacing as space } from '../../spacing';\n",
            ),
            ("other.ts", "import { space } from 'bridge';\nspace(2);\n"),
        ])
    };
    for yaml in [
        "overrides:\n  bridge: link:../bridge\n",
        "overrides:\n  bridge: file:./vendor/bridge\n",
    ] {
        assert_removal(&bridge(("pnpm-workspace.yaml", yaml)), false, yaml);
    }
    // pnpmfile は依存の指定を任意に書き換えられるので、監査を作らず緩和も使わない。
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        (".pnpmfile.cjs", "module.exports = { hooks: {} };\n"),
    ]);
    assert_removal(&repo, false, "pnpmfile");
    // 対照: registry の版だけを指定する YAML なら、registry の依存の緩和を使える。
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        (
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\noverrides:\n  react: ^18.3.0\ncatalog:\n  react: ^18.2.0\n",
        ),
        (
            "package.json",
            "{ \"name\": \"app\", \"dependencies\": { \"react\": \"catalog:\" } }\n",
        ),
    ]);
    assert_removal(&repo, true, "registry versions in the YAML");
}

#[test]
fn trailing_optional_removal_parses_override_keys_by_grammar() {
    // 版の範囲の `>`・`>=`・`<` と、scoped の名前・親子の選択子を正しく読み、registry でない指定で
    // 差し替えた bridge を registry と決めない。
    let repo_with = |package: &str, config: (&str, String)| {
        let manifest =
            format!("{{ \"name\": \"app\", \"dependencies\": {{ \"{package}\": \"^1.0.0\" }} }}\n");
        let mut files: Vec<(String, String)> = vec![
            (".gitignore".to_owned(), "node_modules/\n".to_owned()),
            (
                format!("node_modules/{package}/index.d.ts"),
                "export { spacing as space } from '../../spacing';\n".to_owned(),
            ),
            (
                "other.ts".to_owned(),
                format!("import {{ space }} from '{package}';\nspace(2);\n"),
            ),
        ];
        if config.0 == "package.json" {
            files.push(("package.json".to_owned(), config.1));
        } else {
            files.push(("package.json".to_owned(), manifest));
            files.push((config.0.to_owned(), config.1));
        }
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect();
        spacing_removal_repo(&files)
    };
    let with_overrides = |package: &str, key: &str, spec: &str| {
        format!(
            "{{ \"name\": \"app\", \"dependencies\": {{ \"{package}\": \"^1.0.0\" }}, \"overrides\": {{ \"{key}\": \"{spec}\" }} }}\n"
        )
    };
    for (package, config) in [
        (
            "bridge",
            (
                "package.json",
                with_overrides("bridge", "bridge@>=1.0.0", "link:../bridge"),
            ),
        ),
        (
            "bridge",
            (
                "package.json",
                with_overrides("bridge", "bridge@>1", "file:../bridge"),
            ),
        ),
        (
            "@scope/bridge",
            (
                "package.json",
                with_overrides("@scope/bridge", "@scope/bridge@>=1 <2", "link:../bridge"),
            ),
        ),
        (
            "bridge",
            (
                "pnpm-workspace.yaml",
                "overrides:\n  parent@1>bridge@>=2: link:../bridge\n".to_owned(),
            ),
        ),
    ] {
        let context = format!("{package}: {}", config.1);
        assert_removal(&repo_with(package, config), false, &context);
    }
    // 対照: 範囲を含むキーでも、registry の版だけを指定するなら緩和を使える。
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        (
            "package.json",
            "{ \"name\": \"app\", \"dependencies\": { \"react\": \"^18.2.0\" }, \"overrides\": { \"react@>=18\": \"^18.3.0\" } }\n",
        ),
    ]);
    assert_removal(&repo, true, "registry override with a range key");
}

#[test]
fn trailing_optional_removal_rejects_pnpmfile_settings_in_npmrc() {
    // `.npmrc` の pnpmfile は、別の名前の hook で依存の指定を書き換え得るので、緩和を使わない。
    let repo = spacing_removal_repo(&[
        REACT_IMPORT,
        (".npmrc", "pnpmfile=scripts/install-hooks.cjs\n"),
    ]);
    assert_removal(&repo, false, "pnpmfile in .npmrc");
}
