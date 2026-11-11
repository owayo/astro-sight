//! 末尾 optional 引数追加・const 値のみ変更を降格する互換判定のテスト。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
#[allow(unused_imports)]
use std::collections::HashSet;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::Cursor;
#[allow(unused_imports)]
use std::process::Command;

#[test]
fn is_const_value_only_change_rust_const_value_only_is_true() {
    assert!(is_const_value_only_change(
        "pub const ENEMY_SPEED: f32 = 80.0;",
        "pub const ENEMY_SPEED: f32 = 105.0;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_static_value_only_is_true() {
    assert!(is_const_value_only_change(
        "pub static MAX_ALIVE: usize = 200;",
        "pub static MAX_ALIVE: usize = 280;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_array_value_only_is_true() {
    assert!(is_const_value_only_change(
        "pub const TABLE: [u8; 3] = [1, 2, 3];",
        "pub const TABLE: [u8; 3] = [4, 5, 6];",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_static_mut_is_not_demoted() {
    // mutable storage の初期値は状態契約になりやすいため demote しない。
    assert!(!is_const_value_only_change(
        "pub static mut COUNT: usize = 1;",
        "pub static mut COUNT: usize = 2;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_type_change_stays_api_mod() {
    // 型変更は shape 変更 → 破壊的の可能性があり api.mod に残す。
    assert!(!is_const_value_only_change(
        "pub const X: f32 = 1.0;",
        "pub const X: f64 = 1.0;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_ts_typed_value_only_is_true() {
    assert!(is_const_value_only_change(
        "export const NAME: string = \"a\";",
        "export const NAME: string = \"b\";",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_ts_untyped_scalar_is_true() {
    assert!(is_const_value_only_change(
        "export const MAX = 100;",
        "export const MAX = 200;",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_numeric_arithmetic_is_value_only() {
    for lang in [
        crate::language::LangId::Javascript,
        crate::language::LangId::Typescript,
        crate::language::LangId::Tsx,
    ] {
        for (old, new) in [
            ("60 * 1000", "5 * 60 * 1000"),
            ("1 + 2", "3 - 4"),
            ("1 / 2", "3 % 4"),
            ("2 ** 3", "4 ** 5"),
            ("-(60 * 1000)", "(5 * -60) * 1000"),
            ("0x10 * 2", "0b10 * 0o10 + 1_000"),
            ("1.5e2 * 2", "(.5 + 1) / 2"),
            ("(1 /* old */ + 2)", "(3 + /* new */ 4)"),
        ] {
            assert!(
                is_const_value_only_change(
                    &format!("export const VALUE = {old};"),
                    &format!("export const VALUE = {new};"),
                    "variable",
                    lang,
                ),
                "{lang:?}: {old} -> {new}"
            );
        }
    }
}

#[test]
fn is_const_value_only_change_numeric_arithmetic_keeps_uncertain_types_blocking() {
    for lang in [
        crate::language::LangId::Javascript,
        crate::language::LangId::Typescript,
        crate::language::LangId::Tsx,
    ] {
        for (old, new) in [
            ("60 * 1000", "BASE_MS * 2"),
            ("60 * 1000", "Number('30') * 1000"),
            ("60 * 1000", "'5m'"),
            ("60 * 1000", "1n * 2n"),
            ("60 * 1000", "1n * 2"),
            ("60 * 1000", "1 + '2'"),
            ("60 * 1000", "1 < 2"),
            ("60 * 1000", "1 && 2"),
            ("60 * 1000", "~1 * 2"),
            ("60 * 1000", "[1, 2]"),
            ("60 * 1000", "1 +"),
            // literal 型と number 型の間の変化は既存の blocking 判定に残す。
            ("1000", "30 * 1000"),
            ("30 * 1000", "1000"),
            ("(1000)", "30 * 1000"),
            ("-1000", "30 * 1000"),
        ] {
            assert!(
                !is_const_value_only_change(
                    &format!("export const VALUE = {old};"),
                    &format!("export const VALUE = {new};"),
                    "variable",
                    lang,
                ),
                "{lang:?}: {old} -> {new}"
            );
        }
    }
}

/// 型注釈のない scalar は値から公開型が推論されるため、種類が変わる変更 (number → string 等) は
/// 値だけの変更ではない。静的 container の葉 (`{ timeout: 30 }` → `{ timeout: "30s" }`) が
/// blocking なのと揃え、トップレベルでも種類が同じときだけ const_value に降格する。
#[test]
fn is_const_value_only_change_ts_untyped_scalar_kind_change_stays_api_mod() {
    for lang in [
        crate::language::LangId::Javascript,
        crate::language::LangId::Typescript,
        crate::language::LangId::Tsx,
    ] {
        for (old, new) in [
            ("30", "\"30s\""),
            ("\"30\"", "30"),
            ("true", "'yes'"),
            ("null", "0"),
            ("1", "1n"),
            ("undefined", "null"),
        ] {
            assert!(
                !is_const_value_only_change(
                    &format!("export const TIMEOUT = {old};"),
                    &format!("export const TIMEOUT = {new};"),
                    "variable",
                    lang,
                ),
                "{lang:?}: {old} -> {new}"
            );
        }
        // 対照: 種類が同じなら従来どおり値だけの変更として降格する。
        for (old, new) in [
            ("30", "45"),
            ("\"a\"", "'b'"),
            ("true", "false"),
            ("1n", "2n"),
            ("null", "null"),
        ] {
            assert!(
                is_const_value_only_change(
                    &format!("export const TIMEOUT = {old};"),
                    &format!("export const TIMEOUT = {new};"),
                    "variable",
                    lang,
                ),
                "{lang:?}: {old} -> {new}"
            );
        }
    }
}

#[test]
fn is_const_value_only_change_ts_untyped_function_stays_api_mod() {
    // 型注釈なし + 関数 initializer は shape 推定が危険なため api.mod に残す。
    assert!(!is_const_value_only_change(
        "export const handler = () => 1;",
        "export const handler = () => 2;",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_ts_let_is_not_demoted() {
    assert!(!is_const_value_only_change(
        "export let counter = 1;",
        "export let counter = 2;",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_non_binding_kind_is_false() {
    assert!(!is_const_value_only_change(
        "fn foo() -> i32",
        "fn foo() -> u32",
        "function",
        crate::language::LangId::Rust,
    ));
}

/// Rust の `pub const` / `pub static` の値 (initializer) のみ変更は破壊的でないため、
/// blocking な `modified` ではなく informational な `const_value_changes` に振り分けられる
/// (Issue 2026-06-02-balance-const-value-changes 回帰防止)。
#[test]
fn detect_api_changes_rust_const_value_only_is_demoted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/constants.rs",
            "pub const ENEMY_SPEED: f32 = 80.0;\npub static MAX_ALIVE: usize = 200;\n",
        )],
        "initial",
    );
    // 値のみ変更 (shape 不変)
    fs::write(
        repo.join("src/constants.rs"),
        "pub const ENEMY_SPEED: f32 = 105.0;\npub static MAX_ALIVE: usize = 280;\n",
    )
    .expect("write new constants");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/constants.rs".to_string(),
        new_path: "src/constants.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.is_empty(),
        "値のみ変更の const/static は blocking modified に出すべきでない: {:?}",
        api.modified
    );
    assert!(
        api.const_value_changes
            .iter()
            .any(|c| c.name == "ENEMY_SPEED"),
        "const ENEMY_SPEED の値変更は const_value_changes に出すべき: {:?}",
        api.const_value_changes
    );
    assert!(
        api.const_value_changes
            .iter()
            .any(|c| c.name == "MAX_ALIVE"),
        "static MAX_ALIVE の値変更は const_value_changes に出すべき: {:?}",
        api.const_value_changes
    );
}

#[test]
fn detect_api_changes_rust_internal_const_values_follow_existing_policy() {
    for layout in ["no-cargo", "bin", "private-lib", "public-lib"] {
        for binding in ["pub const", "pub static"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = dir.path();
            init_git_repo_for_test(repo);
            let before = format!("{binding} WINDOW_LIMIT: u64 = 20;\n");
            let root = if layout == "public-lib" {
                "pub mod policy;\npub fn read() -> u64 { policy::WINDOW_LIMIT }\n"
            } else if layout == "private-lib" {
                "mod policy;\npub fn read() -> u64 { policy::WINDOW_LIMIT }\n"
            } else {
                "mod policy;\nfn main() { println!(\"{}\", policy::WINDOW_LIMIT); }\n"
            };
            let root_path = if layout.ends_with("lib") {
                "src/lib.rs"
            } else {
                "src/main.rs"
            };
            let mut base = vec![(root_path, root), ("src/policy.rs", before.as_str())];
            if layout != "no-cargo" {
                base.push((
                    "Cargo.toml",
                    "[package]\nname = \"scan-demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ));
            }
            git_commit_files(repo, &base, "base");
            fs::write(
                repo.join("src/policy.rs"),
                before.replace("= 20", "= 10 * 4"),
            )
            .unwrap();
            let api = detect_api_changes_from_worktree(repo);
            assert!(
                api.const_value_changes
                    .iter()
                    .any(|change| change.name == "WINDOW_LIMIT"),
                "{layout}/{binding}: {api:?}"
            );
            assert!(
                api.added.is_empty() && api.removed.is_empty() && api.modified.is_empty(),
                "{layout}/{binding}: {api:?}"
            );
        }
    }
}

/// `pub const` の型変更 (shape 変更) は const_value_changes ではなく従来どおり
/// blocking な modified に残す。
#[test]
fn detect_api_changes_rust_const_type_change_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("src/constants.rs", "pub const LIMIT: u32 = 10;\n")],
        "initial",
    );
    fs::write(
        repo.join("src/constants.rs"),
        "pub const LIMIT: u64 = 10;\n",
    )
    .expect("write new constants");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/constants.rs".to_string(),
        new_path: "src/constants.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.iter().any(|c| c.name == "LIMIT"),
        "型変更は blocking modified に残すべき: {api:?}"
    );
    assert!(
        api.const_value_changes.is_empty(),
        "型変更は const_value_changes に入れるべきでない: {:?}",
        api.const_value_changes
    );
}

/// Rust の `pub struct` へ private フィールドを追加しただけでは api.mod に出ない。
/// 宣言行 (`pub struct Foo {`) は変わらず、本体 (フィールド) の変更のため
/// `extract_api_signature` が宣言行のみを見る既存のロジックで自然に除外される。
/// (レポート 2026-04-17-private-field-addition-over-detection.md の再現)
#[test]
fn detect_api_changes_private_field_addition_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
#[derive(Debug, Clone)]
pub struct AiService {
    existing: String,
}
";
    git_commit_files(repo, &[("src/lib.rs", before)], "initial");

    // private フィールド追加のみ（pub struct 宣言行は不変）
    let after = "\
#[derive(Debug, Clone)]
pub struct AiService {
    existing: String,
    codex_reasoning_effort: String,
}
";
    fs::write(repo.join("src/lib.rs"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 3,
            old_count: 1,
            new_start: 3,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"AiService"),
        "pub struct の内部（private フィールド）変更は api.mod に出してはならない。got: {mod_names:?}"
    );
}

#[test]
fn detect_api_changes_ts_trailing_optional_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/task-progress.ts",
                "export function computeTaskProgress(phases: string[]): number {\n  return phases.length;\n}\n",
            ),
            (
                "src/components/task-detail.ts",
                "import { computeTaskProgress } from '../lib/task-progress';\nexport const progress = computeTaskProgress([]);\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib/task-progress.ts"),
        "export function computeTaskProgress(phases: string[], description?: string | null): number {\n  return phases.length + (description?.length ?? 0);\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/task-progress.ts".to_string(),
        new_path: "src/lib/task-progress.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.is_empty(),
        "末尾 optional 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "computeTaskProgress")
        .expect("compatible_modified に computeTaskProgress が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

#[test]
fn detect_api_changes_ts_trailing_default_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/task-progress.ts",
                "export function computeTaskProgress(phases: string[]): number {\n  return phases.length;\n}\n",
            ),
            (
                "src/components/task-detail.ts",
                "import { computeTaskProgress } from '../lib/task-progress';\nexport const progress = computeTaskProgress([]);\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib/task-progress.ts"),
        "export function computeTaskProgress(phases: string[], description: string | null = null): number {\n  return phases.length + (description?.length ?? 0);\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/task-progress.ts".to_string(),
        new_path: "src/lib/task-progress.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.is_empty(),
        "末尾 default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "computeTaskProgress")
        .expect("compatible_modified に computeTaskProgress が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Issue 2026-07-20-api-mod-additive-optional-param-overreport: 引数の inline object type
/// literal へ optional プロパティを追加しただけの変更 (本体変更同時可) は後方互換のため、
/// blocking な api.mod ではなく compatible_modified (`optional_object_props`) に降格する。
#[test]
fn detect_api_changes_ts_optional_object_prop_addition_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/domain/decide.ts",
                "export type Result = { ok: boolean };\n\nexport function decide(input: { a: number; b: number }): Result {\n  return { ok: true };\n}\n",
            ),
            (
                "src/caller.ts",
                "import { decide } from './domain/decide';\nexport const outcome = decide({ a: 1, b: 2 });\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/domain/decide.ts"),
        "export type Result = { ok: boolean };\n\nexport function decide(input: {\n  a: number;\n  b: number;\n  cap?: number;\n}): Result {\n  const { a, cap = 100 } = input;\n  if (a > cap) return { ok: false };\n  return { ok: true };\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/domain/decide.ts".to_string(),
        new_path: "src/domain/decide.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 11,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api_changes.modified.iter().any(|m| m.name == "decide"),
        "object type への optional プロパティ追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "decide")
        .expect("compatible_modified に decide が入るべき");
    assert_eq!(compat.reason, "optional_object_props");
}

#[test]
fn detect_api_changes_ts_optional_union_object_props_preserve_other_contracts() {
    let cases = [
        (
            "inline",
            "options: { mode: string }",
            "options: { mode: string; label?: string }",
            true,
        ),
        (
            "optional inline",
            "options?: { mode: string }",
            "options?: { mode: string; label?: string }",
            true,
        ),
        (
            "default inline",
            "options: { mode: string } = { mode: 'fast' }",
            "options: { mode: string; label?: string } = { mode: 'fast' }",
            true,
        ),
        (
            "undefined",
            "options: { mode: string } | undefined",
            "options: { mode: string; label?: string } | undefined",
            true,
        ),
        (
            "null",
            "options: { mode: string } | null",
            "options: { mode: string; label?: string } | null",
            true,
        ),
        (
            "default union",
            "options: { mode: string } | undefined = undefined",
            "options: { mode: string; label?: string } | undefined = undefined",
            true,
        ),
        (
            "optional union",
            "options?: { mode: string } | null",
            "options?: { mode: string; label?: string } | null",
            true,
        ),
        (
            "middle object",
            "options: undefined | { mode: string } | null",
            "options: undefined | { mode: string; label?: string } | null",
            true,
        ),
        (
            "leading pipe",
            "options: { mode: string } | undefined",
            "options:\n | { mode: string; label?: string }\n | undefined",
            true,
        ),
        (
            "other branch changed",
            "options: { mode: string } | undefined",
            "options: { mode: string; label?: string } | null",
            false,
        ),
        (
            "branch added",
            "options: { mode: string } | undefined",
            "options: { mode: string; label?: string } | undefined | null",
            false,
        ),
        (
            "branch removed",
            "options: { mode: string } | undefined | null",
            "options: { mode: string; label?: string } | undefined",
            false,
        ),
        (
            "branch reordered",
            "options: { mode: string } | undefined",
            "options: undefined | { mode: string; label?: string }",
            false,
        ),
        (
            "multiple objects",
            "options: { mode: string } | { size: number }",
            "options: { mode: string; label?: string } | { size: number }",
            false,
        ),
        (
            "intersection",
            "options: { mode: string } & Extra",
            "options: { mode: string; label?: string } & Extra",
            false,
        ),
        (
            "intersection branch",
            "options: { mode: string } | (Extra & Other)",
            "options: { mode: string; label?: string } | (Extra & Other)",
            false,
        ),
        (
            "parenthesized object",
            "options: ({ mode: string }) | undefined",
            "options: ({ mode: string; label?: string }) | undefined",
            false,
        ),
        (
            "parenthesized union",
            "options: ({ mode: string } | undefined)",
            "options: ({ mode: string; label?: string } | undefined)",
            false,
        ),
        (
            "generic wrapper",
            "options: Array<{ mode: string } | undefined>",
            "options: Array<{ mode: string; label?: string } | undefined>",
            false,
        ),
        (
            "required addition",
            "options: { mode: string } | undefined",
            "options: { mode: string; label: string } | undefined",
            false,
        ),
        (
            "existing member changed",
            "options: { mode: string } | undefined",
            "options: { mode: number; label?: string } | undefined",
            false,
        ),
        (
            "kind changed",
            "options: { mode: string } | undefined",
            "options?: { mode: string; label?: string } | undefined",
            false,
        ),
        (
            "pattern changed",
            "options: { mode: string } | undefined",
            "changed: { mode: string; label?: string } | undefined",
            false,
        ),
        (
            "default added",
            "options: { mode: string } | undefined",
            "options: { mode: string; label?: string } | undefined = undefined",
            false,
        ),
        (
            "default changed",
            "options: { mode: string } | null | undefined = undefined",
            "options: { mode: string; label?: string } | null | undefined = null",
            false,
        ),
        (
            "literal whitespace changed",
            "options: { mode: string } | 'a b'",
            "options: { mode: string; label?: string } | 'a  b'",
            false,
        ),
        (
            "scalar branch",
            "options: { mode: string } | string",
            "options: { mode: string; label?: string } | string",
            true,
        ),
        (
            "union comment preserved",
            "options: { mode: string } | /* retained */ undefined",
            "options: { mode: string; label?: string } | /* retained */ undefined",
            true,
        ),
        (
            "union comment changed",
            "options: { mode: string } | /* old */ undefined",
            "options: { mode: string; label?: string } | /* new */ undefined",
            false,
        ),
        (
            "incomplete member",
            "options: { mode: string } | undefined",
            "options: { mode: string; label?: } | undefined",
            false,
        ),
    ];
    for extension in ["ts", "tsx"] {
        for (label, old_param, new_param, compatible) in cases {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = dir.path();
            init_git_repo_for_test(repo);
            let file = format!("api.{extension}");
            let before = format!(
                "export function save(name: string, {old_param}): string {{ return name; }}\n"
            );
            let after = format!(
                "export function save(name: string, {new_param}): string {{ return name; }}\n"
            );
            git_commit_files(repo, &[(&file, &before)], "initial");
            fs::write(repo.join(&file), after).expect("write changed file");
            let output = Command::new("git")
                .args(["diff", "--unified=0", "HEAD"])
                .current_dir(repo)
                .output()
                .expect("git diff");
            assert!(output.status.success());
            let diff_files = crate::engine::diff::parse_unified_diff(
                std::str::from_utf8(&output.stdout).expect("utf-8 diff"),
            );
            let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
            assert_eq!(
                api.compatible_modified.iter().any(|change| change.name == "save" && change.reason == "optional_object_props"),
                compatible,
                "{extension} {label}: {api:?}"
            );
            assert_eq!(
                api.modified.iter().any(|change| change.name == "save"),
                !compatible,
                "{extension} {label}: {api:?}"
            );
        }
    }
}

#[test]
fn detect_api_changes_ts_destructured_optional_props_keep_the_type_contract() {
    for extension in ["ts", "tsx"] {
        for (old_param, new_param, compatible) in [
            (
                "{ label }: { label: string }",
                "{ label }: { label: string; hint?: string }",
                true,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint }: { label: string; hint?: string }",
                true,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint = '' }: { label: string; hint?: string }",
                true,
            ),
            (
                "{ label }: { label: string } | undefined = { label: '' }",
                "{ label, hint }: { label: string; hint?: string } | undefined = { label: '' }",
                true,
            ),
            (
                "{ label }: { label: string; hint?: string }",
                "{ label, hint }: { label: string; hint?: string; extra?: boolean }",
                false,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint }: { label: string; hint: string }",
                false,
            ),
            (
                "{ label, ...rest }: { label: string }",
                "{ label, hint, ...rest }: { label: string; hint?: string }",
                false,
            ),
            (
                "{ label: hint }: { label: string }",
                "{ label: hint, hint }: { label: string; hint?: string }",
                false,
            ),
            (
                "{ label }: { label: string } | Base",
                "{ label }: { label: string; hint?: string } | Base",
                true,
            ),
            (
                "{ label }: { label: string } | Base",
                "{ label, hint }: { label: string; hint?: string } | Base",
                false,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint = '' }: { label: string; hint?: number }",
                false,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint = null }: { label: string; hint?: string }",
                false,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint = `x` }: { label: string; hint?: 'a' | 'b' }",
                false,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint = 1n }: { label: string; hint?: number }",
                false,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint = -1n }: { label: string; hint?: number }",
                false,
            ),
            (
                "{ label }: { label: string }",
                "{ label, hint = -1 }: { label: string; hint?: string | number | null }",
                true,
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let repo = temporary.path();
            init_git_repo_for_test(repo);
            let file = format!("api.{extension}");
            let before = format!(
                "type Base = {{ label: string; mode: string }};\nexport function Field({old_param}): string {{ return ''; }}\n"
            );
            let after = format!(
                "type Base = {{ label: string; mode: string }};\nexport function Field({new_param}): string {{ return ''; }}\n"
            );
            git_commit_files(repo, &[(&file, &before)], "base");
            fs::write(repo.join(&file), after).unwrap();
            let output = Command::new("git")
                .args(["diff", "--unified=0", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(output.status.success());
            let diff = crate::engine::diff::parse_unified_diff(
                std::str::from_utf8(&output.stdout).unwrap(),
            );
            let api = detect_api_changes(repo.to_str().unwrap(), "HEAD", &diff);
            assert_eq!(
                api.compatible_modified
                    .iter()
                    .any(|item| item.name == "Field" && item.reason == "optional_object_props"),
                compatible,
                "{extension}: {old_param} -> {new_param}: {api:?}"
            );
            assert_eq!(
                api.modified.iter().any(|item| item.name == "Field"),
                !compatible,
                "{api:?}"
            );
        }
    }
}

#[test]
fn detect_api_changes_ts_destructured_optional_props_body_bindings_preserve_scope() {
    for extension in ["ts", "tsx"] {
        for (body, compatible) in [
            ("const hint = label; return hint;", false),
            ("{ const hint = label; } return label;", true),
            ("var hint = label; return hint;", false),
            ("function hint() {} return label;", false),
            ("if (label) { var hint = label; } return label;", false),
            ("for (var hint in data) {} return label;", false),
            ("var local = label; return local;", true),
            (
                "const f = function () { var hint = ''; }; return label;",
                true,
            ),
            ("return hint || label;", true),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let repo = temporary.path();
            init_git_repo_for_test(repo);
            let file = format!("api.{extension}");
            let before = format!(
                "const hint = '';\nexport function Field({{ label }}: {{ label: string }}): string {{ {body} }}\n"
            );
            let after = format!(
                "const hint = '';\nexport function Field({{ label, hint }}: {{ label: string; hint?: string }}): string {{ {body} }}\n"
            );
            git_commit_files(repo, &[(&file, &before)], "base");
            fs::write(repo.join(&file), after).unwrap();
            let output = Command::new("git")
                .args(["diff", "--unified=0", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(output.status.success());
            let diff = crate::engine::diff::parse_unified_diff(
                std::str::from_utf8(&output.stdout).unwrap(),
            );
            let api = detect_api_changes(repo.to_str().unwrap(), "HEAD", &diff);
            assert_eq!(
                api.compatible_modified
                    .iter()
                    .any(|item| item.name == "Field" && item.reason == "optional_object_props"),
                compatible,
                "{extension}/{body}: {api:?}"
            );
            assert_eq!(
                api.modified.iter().any(|item| item.name == "Field"),
                !compatible,
                "{extension}/{body}: {api:?}"
            );
        }
    }
}

/// 負ケース: 引数 object type への必須プロパティ追加は既存呼び出しを壊すため
/// blocking な api.mod を維持する。
#[test]
fn detect_api_changes_ts_required_object_prop_addition_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/domain/decide.ts",
                "export type Result = { ok: boolean };\n\nexport function decide(input: { a: number; b: number }): Result {\n  return { ok: true };\n}\n",
            ),
            (
                "src/caller.ts",
                "import { decide } from './domain/decide';\nexport const outcome = decide({ a: 1, b: 2 });\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/domain/decide.ts"),
        "export type Result = { ok: boolean };\n\nexport function decide(input: { a: number; b: number; cap: number }): Result {\n  return { ok: input.a > input.cap };\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/domain/decide.ts".to_string(),
        new_path: "src/domain/decide.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.iter().any(|m| m.name == "decide"),
        "必須プロパティ追加は blocking な api.mod を維持すべき: modified={:?} compat={:?}",
        api_changes.modified,
        api_changes.compatible_modified
    );
}

/// 負ケース: optional プロパティ追加と同時に既存プロパティの型を変更した場合は
/// 既存呼び出しが壊れ得るため blocking な api.mod を維持する。
#[test]
fn detect_api_changes_ts_object_prop_type_change_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/domain/decide.ts",
                "export type Result = { ok: boolean };\n\nexport function decide(input: { a: number; b: number }): Result {\n  return { ok: true };\n}\n",
            ),
            (
                "src/caller.ts",
                "import { decide } from './domain/decide';\nexport const outcome = decide({ a: 1, b: 2 });\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/domain/decide.ts"),
        "export type Result = { ok: boolean };\n\nexport function decide(input: { a: string; b: number; cap?: number }): Result {\n  return { ok: true };\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/domain/decide.ts".to_string(),
        new_path: "src/domain/decide.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.iter().any(|m| m.name == "decide"),
        "既存プロパティの型変更は blocking な api.mod を維持すべき: modified={:?} compat={:?}",
        api_changes.modified,
        api_changes.compatible_modified
    );
}

/// Issue 2026-07-20-react-rsc-async-component-impact-classification: React Server Component
/// の async 化 (async キーワード追加のみ、Props 不変) で参照が JSX タグ利用のみなら、
/// 呼び出し側の書き換えが不要なため compatible_modified (`async_jsx_component`) に降格する。
#[test]
fn detect_api_changes_tsx_async_jsx_component_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "web/components/SiteHeader.tsx",
                "export type SiteHeaderProps = { user: string };\n\nexport function SiteHeader(props: SiteHeaderProps) {\n  return <header>{props.user}</header>;\n}\n",
            ),
            (
                "web/app/layout.tsx",
                "import { SiteHeader } from \"../components/SiteHeader\";\n\nexport default async function RootLayout({ children }: { children: any }) {\n  return (\n    <html>\n      <body>\n        <SiteHeader user=\"alice\" />\n        {children}\n      </body>\n    </html>\n  );\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("web/components/SiteHeader.tsx"),
        "export type SiteHeaderProps = { user: string };\n\nasync function fetchGreeting(): Promise<string> {\n  return \"hi\";\n}\n\nexport async function SiteHeader(props: SiteHeaderProps) {\n  const greeting = await fetchGreeting();\n  return <header>{props.user}{greeting}</header>;\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "web/components/SiteHeader.tsx".to_string(),
        new_path: "web/components/SiteHeader.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api_changes.modified.iter().any(|m| m.name == "SiteHeader"),
        "JSX 利用のみの RSC async 化は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "SiteHeader")
        .expect("compatible_modified に SiteHeader が入るべき");
    assert_eq!(compat.reason, "async_jsx_component");
}

/// 負ケース: async 化されたコンポーネントが関数として直接呼び出されている場合、戻り値が
/// Promise になり await が必要になるため blocking な api.mod を維持する。
#[test]
fn detect_api_changes_tsx_async_component_with_call_usage_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "web/components/SiteHeader.tsx",
                "export type SiteHeaderProps = { user: string };\n\nexport function SiteHeader(props: SiteHeaderProps) {\n  return <header>{props.user}</header>;\n}\n",
            ),
            (
                "web/app/layout.tsx",
                "import { SiteHeader } from \"../components/SiteHeader\";\n\nexport default async function RootLayout({ children }: { children: any }) {\n  const rendered = SiteHeader({ user: \"alice\" });\n  return (\n    <html>\n      <body>\n        {rendered}\n        {children}\n      </body>\n    </html>\n  );\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("web/components/SiteHeader.tsx"),
        "export type SiteHeaderProps = { user: string };\n\nexport async function SiteHeader(props: SiteHeaderProps) {\n  return <header>{props.user}</header>;\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "web/components/SiteHeader.tsx".to_string(),
        new_path: "web/components/SiteHeader.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.iter().any(|m| m.name == "SiteHeader"),
        "関数呼び出し利用が残る async 化は blocking を維持すべき: modified={:?} compat={:?}",
        api_changes.modified,
        api_changes.compatible_modified
    );
}

/// 負ケース: `"use client"` directive を持つ Client Component の async 化は React の
/// ランタイムエラーになる破壊的変更のため blocking な api.mod を維持する。
#[test]
fn detect_api_changes_tsx_async_use_client_component_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "web/components/SiteHeader.tsx",
                "\"use client\";\n\nexport type SiteHeaderProps = { user: string };\n\nexport function SiteHeader(props: SiteHeaderProps) {\n  return <header>{props.user}</header>;\n}\n",
            ),
            (
                "web/app/layout.tsx",
                "import { SiteHeader } from \"../components/SiteHeader\";\n\nexport default async function RootLayout({ children }: { children: any }) {\n  return (\n    <html>\n      <body>\n        <SiteHeader user=\"alice\" />\n        {children}\n      </body>\n    </html>\n  );\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("web/components/SiteHeader.tsx"),
        "\"use client\";\n\nexport type SiteHeaderProps = { user: string };\n\nexport async function SiteHeader(props: SiteHeaderProps) {\n  return <header>{props.user}</header>;\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "web/components/SiteHeader.tsx".to_string(),
        new_path: "web/components/SiteHeader.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 7,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.iter().any(|m| m.name == "SiteHeader"),
        "use client コンポーネントの async 化は blocking を維持すべき: modified={:?} compat={:?}",
        api_changes.modified,
        api_changes.compatible_modified
    );
}

/// Issue 2026-07-12-ts-class-method-trailing-optional-param-api-mod: TS class method への
/// 末尾 optional 引数追加も compatible_modified へ降格する。同名 standalone 関数が別
/// ファイルにあっても `Class.method` qualname で一意解決できるため成立する。
#[test]
fn detect_api_changes_ts_class_method_trailing_optional_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/widget.ts",
                "export class Widget {\n  handle(a: string, b: number): void {\n    console.log(a, b);\n  }\n}\n",
            ),
            (
                "src/user.ts",
                "import { Widget } from './widget';\n\nexport function run(w: Widget) {\n  w.handle(\"x\", 1);\n}\n",
            ),
            (
                "src/other/standalone.ts",
                "export function handle(msg: string): void {\n  console.log(msg);\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/widget.ts"),
        "export class Widget {\n  handle(a: string, b: number, c?: readonly number[]): void {\n    console.log(a, b, c);\n  }\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api_changes
            .modified
            .iter()
            .any(|m| m.name == "Widget.handle"),
        "class method への末尾 optional 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "Widget.handle")
        .expect("compatible_modified に Widget.handle が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// 負ケース: class method へ追加した末尾引数が required なら従来どおり blocking。
#[test]
fn detect_api_changes_ts_class_method_trailing_required_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/widget.ts",
                "export class Widget {\n  handle(a: string, b: number): void {\n    console.log(a, b);\n  }\n}\n",
            ),
            (
                "src/user.ts",
                "import { Widget } from './widget';\n\nexport function run(w: Widget) {\n  w.handle(\"x\", 1);\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/widget.ts"),
        "export class Widget {\n  handle(a: string, b: number, c: readonly number[]): void {\n    console.log(a, b, c);\n  }\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes
            .modified
            .iter()
            .any(|m| m.name == "Widget.handle"),
        "required 引数の追加は blocking な modified を維持すべき: modified={:?} compat={:?}",
        api_changes
            .modified
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>(),
        api_changes
            .compatible_modified
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}

/// 負ケース: 新側に同名 overload signature が併存する場合は単一 method_definition へ
/// 安全に対応付けられないため blocking を維持する。
#[test]
fn detect_api_changes_ts_class_method_with_overload_signature_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/widget.ts",
                "export class Widget {\n  handle(a: string, b: number): void {\n    console.log(a, b);\n  }\n}\n",
            ),
            (
                "src/user.ts",
                "import { Widget } from './widget';\n\nexport function run(w: Widget) {\n  w.handle(\"x\", 1);\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/widget.ts"),
        "export class Widget {\n  handle(a: string, b: number): void;\n  handle(a: string, b: number, c?: readonly number[]): void;\n  handle(a: string, b: number, c?: readonly number[]): void {\n    console.log(a, b, c);\n  }\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api_changes
            .compatible_modified
            .iter()
            .any(|c| c.name == "Widget.handle"),
        "overload signature 併存時は互換降格しない: compat={:?}",
        api_changes
            .compatible_modified
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}

/// Python のトップレベル関数に末尾 keyword-only + default 引数を追加した変更は
/// 既存呼び出しを壊さないため、`compatible_modified` (`trailing_optional_params`) に降格する。
/// (レポート 2026-06-18-api-mod-backward-compatible-kwarg の再現)
#[test]
fn detect_api_changes_python_trailing_kwonly_default_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/uploader.py",
                "def upload_category_spreadsheets(items):\n    return items\n",
            ),
            (
                "src/main.py",
                "from uploader import upload_category_spreadsheets\n\nresult = upload_category_spreadsheets([])\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/uploader.py"),
        "def upload_category_spreadsheets(items, *, max_spreadsheet_bytes=600_000):\n    return items\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/uploader.py".to_string(),
        new_path: "src/uploader.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.is_empty(),
        "末尾 kwonly+default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "upload_category_spreadsheets")
        .expect("compatible_modified に upload_category_spreadsheets が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Python トップレベル関数の末尾 positional default 引数追加も同様に compatible_modified に降格する。
#[test]
fn detect_api_changes_python_trailing_default_positional_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/lib/helpers.py",
            "def render(items):\n    return items\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("src/lib/helpers.py"),
        "def render(items, limit=10):\n    return items[:limit]\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/helpers.py".to_string(),
        new_path: "src/lib/helpers.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.is_empty(),
        "末尾 default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "render")
        .expect("compatible_modified に render が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Python モジュール直下クラスのメソッドに末尾 kwonly+default 引数を追加した変更も
/// `compatible_modified` (`trailing_optional_params`) に降格する。
#[test]
fn detect_api_changes_python_class_method_trailing_kwonly_default_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/svc.py",
                "class Service:\n    def emit(self, payload):\n        return payload\n",
            ),
            (
                "src/main.py",
                "from svc import Service\n\nresult = Service().emit({})\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/svc.py"),
        "class Service:\n    def emit(self, payload, *, retry=0):\n        return payload\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/svc.py".to_string(),
        new_path: "src/svc.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes
            .modified
            .iter()
            .all(|c| c.name != "Service.emit"),
        "末尾 kwonly+default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "Service.emit")
        .expect("compatible_modified に Service.emit が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Python デコレータが変わった場合 (例: `@staticmethod` → `@classmethod`) は
/// default 引数追加が併走しても compatible_modified に降格せず modified に残るべき。
/// (呼び出し時の cls / self bind が変わり既存呼出を壊しうるため)
#[test]
fn detect_api_changes_python_decorator_change_with_optional_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/svc.py",
                "class Service:\n    @staticmethod\n    def emit(payload):\n        return payload\n",
            ),
            (
                "src/main.py",
                "from svc import Service\n\nresult = Service.emit({})\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/svc.py"),
        "class Service:\n    @classmethod\n    def emit(cls, payload, *, retry=0):\n        return payload\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/svc.py".to_string(),
        new_path: "src/svc.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes
            .compatible_modified
            .iter()
            .all(|c| c.name != "Service.emit"),
        "デコレータ変更 + default 引数追加は compatible_modified に降格してはならない: {:?}",
        api_changes.compatible_modified
    );
}

/// Python の必須引数 (default 無し) 追加は依然 modified (api.mod) として残るべき。
#[test]
fn detect_api_changes_python_trailing_required_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/lib/helpers.py",
            "def render(items):\n    return items\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("src/lib/helpers.py"),
        "def render(items, limit):\n    return items[:limit]\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/helpers.py".to_string(),
        new_path: "src/lib/helpers.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.compatible_modified.is_empty(),
        "required 引数追加を compatible_modified に降格してはならない: {:?}",
        api_changes.compatible_modified
    );
    assert!(
        api_changes.modified.iter().any(|c| c.name == "render"),
        "required 引数追加は api.mod に残るべき: {:?}",
        api_changes.modified
    );
}

#[test]
fn detect_api_changes_ts_trailing_required_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/task-progress.ts",
                "export function computeTaskProgress(phases: string[]): number {\n  return phases.length;\n}\n",
            ),
            (
                "src/components/task-detail.ts",
                "import { computeTaskProgress } from '../lib/task-progress';\nexport const progress = computeTaskProgress([]);\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib/task-progress.ts"),
        "export function computeTaskProgress(phases: string[], description: string): number {\n  return phases.length + description.length;\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/task-progress.ts".to_string(),
        new_path: "src/lib/task-progress.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.compatible_modified.is_empty(),
        "required 引数追加を compatible_modified に降格してはならない: {:?}",
        api_changes.compatible_modified
    );
    assert!(
        api_changes
            .modified
            .iter()
            .any(|c| c.name == "computeTaskProgress"),
        "required 引数追加は api.mod に残るべき: {:?}",
        api_changes.modified
    );
}

/// 後方互換なオプショナル引数の追加（末尾にデフォルト値付き引数を追加）は、
/// closed-in-diff 判定により api.mod から除外される。
/// (レポート追記 2026-04-22 コミット c045fdf `json_to_markdown` の再現)
#[test]
fn detect_api_changes_optional_arg_addition_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
def json_to_markdown(raw, impact_file=None):
    return str(raw)


def _finalize_result(raw):
    return json_to_markdown(raw)


if __name__ == \"__main__\":
    _finalize_result({})
";
    git_commit_files(repo, &[("review_mr.py", before)], "initial");

    let after = "\
def json_to_markdown(raw, impact_file=None, osv_scan_file=None):
    return str(raw)


def _finalize_result(raw):
    return json_to_markdown(raw, impact_file=None, osv_scan_file=None)


if __name__ == \"__main__\":
    _finalize_result({})
";
    fs::write(repo.join("review_mr.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "review_mr.py".to_string(),
        new_path: "review_mr.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"json_to_markdown"),
        "同一ファイル内でのみ呼ばれる関数へのオプショナル引数追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// 値バインディングの signature からコメントトークンを落とす。
///
/// signature は初期化子を含む item 全体から作るため、旧実装ではコメントまで signature の
/// 一部になり、**コードを 1 文字も変えずコメントだけを直すと blocking な `api.mod` に載って
/// Stop hook が止まっていた** (Issue: 2026-09-16-const-array-append-and-cochange-fp の
/// パターン A、実測 exit 1)。加えて `normalize_signature_whitespace` が改行を潰すため
/// 行コメント以降が 1 行に連なり、shape 抽出が parse に失敗して fail-closed で blocking 側に
/// 残るという二次被害もあった。
///
/// **対照ケース内蔵** (「何も検出しなくなっただけ」の退行を防ぐ):
/// - `TYPE_CHANGED`: コメント変更と同時に型も変える → 従来どおり blocking な `modified`
/// - `VALUE_CHANGED`: コメント変更と同時に値も変える → informational な `const_value_changes`
/// - `URL`: 文字列リテラル中の `//` を変える → コメントと誤認せず値変更として検出
#[test]
fn detect_api_changes_rust_comment_only_change_is_not_an_api_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/constants.rs",
            "/// 一覧表\n\
pub const TABLE: &[&str] = &[\n\
    // 最初の要素\n\
    \"a\",\n\
    /* 二番目 */ \"b\",\n\
];\n\
pub const TYPE_CHANGED: &[u32] = &[\n    // 旧コメント\n    1,\n];\n\
pub const VALUE_CHANGED: &[u32] = &[\n    // 旧コメント\n    1,\n];\n\
pub const URL: &str = \"https://example.test/a//b\";\n",
        )],
        "initial",
    );
    // TABLE: コメントだけを変える (配列の中身は不変)。
    // TYPE_CHANGED: コメント + 型変更。VALUE_CHANGED: コメント + 値変更。
    // URL: 文字列リテラル中の `//` の後ろを変更 (コメントではない)。
    fs::write(
        repo.join("src/constants.rs"),
        "/// 一覧表 (仕様 X に対応)\n\
pub const TABLE: &[&str] = &[\n\
    // 先頭の要素 — 既定値として使う\n\
    \"a\",\n\
    /* 2 番目の要素 */ \"b\",\n\
];\n\
pub const TYPE_CHANGED: &[u64] = &[\n    // 新コメント (型も変えた)\n    1,\n];\n\
pub const VALUE_CHANGED: &[u32] = &[\n    // 新コメント (値も変えた)\n    2,\n];\n\
pub const URL: &str = \"https://example.test/a//c\";\n",
    )
    .expect("write new constants");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/constants.rs".to_string(),
        new_path: "src/constants.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 15,
            new_start: 1,
            new_count: 15,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    // コメントだけを変えた TABLE はどの bucket にも出ない
    for (bucket, names) in [
        (
            "modified",
            api.modified
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "const_value_changes",
            api.const_value_changes
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "added",
            api.added
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "removed",
            api.removed
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
        ),
    ] {
        assert!(
            !names.contains(&"TABLE"),
            "コメントのみ変更した TABLE が {bucket} に出ている: {names:?}"
        );
    }

    // 対照 1: 型変更は従来どおり blocking な modified に残る
    assert!(
        api.modified.iter().any(|m| m.name == "TYPE_CHANGED"),
        "型変更は modified に残すべき: {:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    // 対照 2: 値変更は informational な const_value_changes に入る
    assert!(
        api.const_value_changes
            .iter()
            .any(|c| c.name == "VALUE_CHANGED"),
        "値変更は const_value_changes に出すべき: {:?}",
        api.const_value_changes
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
    // 対照 3: 文字列リテラル中の `//` はコメントではない。値変更として拾う
    assert!(
        api.const_value_changes.iter().any(|c| c.name == "URL"),
        "文字列中の // をコメントと誤認せず値変更として拾うべき: {:?}",
        api.const_value_changes
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}

/// JS/TS では JSDoc (`/** @type {...} */`) が型アサーションとして実際に型契約を持つため、
/// signature から落とさない。落とすと型の絞り込み・拡大を見逃す fail-open になる。
///
/// **対照ケース内蔵**: 同じファイルの通常コメント (`//` / 非 JSDoc の `/* */`) だけを
/// 変えた定数は、Rust と同じくどの bucket にも出ないことを固定する。
#[test]
fn detect_api_changes_ts_keeps_jsdoc_but_drops_plain_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/values.ts",
            "export const TYPED = /** @type {string | number} */ (\"x\");\n\
export const PLAIN: string[] = [\n  // 説明\n  \"y\",\n];\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("src/values.ts"),
        "export const TYPED = /** @type {string} */ (\"x\");\n\
export const PLAIN: string[] = [\n  // 説明を書き直した\n  \"y\",\n];\n",
    )
    .expect("write new values");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/values.ts".to_string(),
        new_path: "src/values.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    // JSDoc の型を狭めた TYPED は **blocking な modified** に残り続ける。
    // modified と const_value_changes を結合して判定すると、将来 informational 側へ
    // 誤降格しても通ってしまうので、bucket を分けて確認する。
    let modified: Vec<&str> = api.modified.iter().map(|m| m.name.as_str()).collect();
    let demoted: Vec<&str> = api
        .const_value_changes
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        modified.contains(&"TYPED"),
        "JSDoc の型変更は blocking な modified に残すべき: modified={modified:?} demoted={demoted:?}"
    );
    assert!(
        !demoted.contains(&"TYPED"),
        "JSDoc の型変更を const_value_changes へ降格してはいけない: {demoted:?}"
    );
    // 対照: 通常コメントだけの変更はどちらにも出さない
    assert!(
        !modified.contains(&"PLAIN") && !demoted.contains(&"PLAIN"),
        "通常コメントのみ変更した PLAIN は API 変更に出すべきでない: modified={modified:?} demoted={demoted:?}"
    );
}

/// base の `files` をコミットし、作業ツリーへ `changes` を書いてから、`name` の API 差分の区分を
/// 返す (どこにも無ければ "none")。
fn bucket_after(files: &[(&str, &str)], changes: &[(&str, &str)], name: &str) -> &'static str {
    bucket_after_removing(files, changes, &[], name)
}

/// [`bucket_after`] に加えて、作業ツリーから `removed` のファイルを消す。
fn bucket_after_removing(
    files: &[(&str, &str)],
    changes: &[(&str, &str)],
    removed: &[&str],
    name: &str,
) -> &'static str {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, files, "base");
    for (path, content) in changes {
        fs::write(repo.join(path), content).expect("write");
    }
    for path in removed {
        fs::remove_file(repo.join(path)).expect("remove");
    }
    let api = detect_api_changes_from_worktree(repo);
    if api.modified.iter().any(|c| c.name == name) {
        "mod"
    } else if api.modified_closed_in_diff.iter().any(|c| c.name == name) {
        "mod_closed"
    } else if api
        .compatible_modified
        .iter()
        .any(|c| c.name == name && c.reason == "optional_object_props")
    {
        "mod_compat"
    } else {
        "none"
    }
}

const PRICE_PACKAGE: &str = "{ \"name\": \"repro\", \"private\": true, \"type\": \"module\", \"dependencies\": { \"react\": \"19.0.0\" }, \"devDependencies\": { \"@types/react\": \"19.0.0\" } }\n";
const PRICE_OLD: &str =
    "export function Price({ amount }: { amount: number }) {\n\treturn <span>{amount}</span>;\n}\n";
const PRICE_CART: &str = "import { Price } from \"./price\";\n\nexport function Cart() {\n\treturn <Price amount={1200} />;\n}\n";

/// 関数部品の props の型リテラルに足した省略可能なプロパティの前のコメント (JSDoc・行コメント・
/// メンバーの中のコメント) は型を変えないので、コメントが無いときと同じく
/// `optional_object_props` の互換 (Issue #265)。既存のメンバーのコメントだけの変更も互換。
/// 宣言の出力を変える `@internal` と、診断を変える `@ts-` の指示が付けば従来どおり blocking
/// (対照)。
#[test]
fn ts_optional_object_prop_comments_keep_compatibility() {
    let added = |comment: &str, member: &str| {
        format!(
            "export function Price({{\n\tamount,\n\tcurrency = \"JPY\",\n}}: {{\n\tamount: number;\n{comment}\t{member};\n}}) {{\n\treturn (\n\t\t<span>\n\t\t\t{{amount}} {{currency}}\n\t\t</span>\n\t);\n}}\n"
        )
    };
    for (price_new, expected) in [
        (added("", "currency?: string"), "mod_compat"),
        (
            added("\t/** ISO 4217 currency code */\n", "currency?: string"),
            "mod_compat",
        ),
        (
            added("\t// ISO 4217 currency code\n", "currency?: string"),
            "mod_compat",
        ),
        (
            added("", "currency /* ISO 4217 */ ?: string"),
            "mod_compat",
        ),
        (
            "export function Price({ amount }: {\n\t/** price in yen */\n\tamount: number;\n}) {\n\treturn <span>{amount}</span>;\n}\n".to_string(),
            "mod_compat",
        ),
        (
            added("\t/** @internal */\n", "currency?: string"),
            "mod",
        ),
        (
            added("\t// @ts-expect-error\n", "currency?: string"),
            "mod",
        ),
    ] {
        assert_eq!(
            bucket_after(
                &[
                    ("package.json", PRICE_PACKAGE),
                    ("tsconfig.json", "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" } }\n"),
                    ("src/price.tsx", PRICE_OLD),
                    ("src/cart.tsx", PRICE_CART),
                ],
                &[("src/price.tsx", &price_new)],
                "Price",
            ),
            expected,
            "{price_new}"
        );
    }
}

/// 必須プロパティの追加の証明 (呼び出し側の共有 const を同じ差分で直した) も、足した
/// プロパティの前のコメントを数えない。`@internal` が付けば証明しない (対照)。
#[test]
fn ts_required_object_prop_comment_keeps_shared_const_closure() {
    let build = |comment: &str| {
        format!(
            "export function buildSql(deps: {{\n\tevents: string;\n\tusers: string;\n{comment}\tgroups: string;\n}}): string {{\n\treturn `${{deps.events}}:${{deps.users}}:${{deps.groups}}`;\n}}\n"
        )
    };
    let caller_after = "import { buildSql } from \"./build\";\n\nconst SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n";
    for (comment, expected) in [
        ("", "mod_closed"),
        ("\t/** grouping key */\n", "mod_closed"),
        ("\t/** @internal */\n", "mod"),
    ] {
        let build_new = build(comment);
        assert_eq!(
            bucket_after(
                &[
                    (
                        "src/build.ts",
                        "export function buildSql(deps: {\n\tevents: string;\n\tusers: string;\n}): string {\n\treturn `${deps.events}:${deps.users}`;\n}\n",
                    ),
                    ("src/build.test.ts", TS_SHARED_CONST_CALLER_BEFORE),
                ],
                &[
                    ("src/build.ts", &build_new),
                    ("src/build.test.ts", caller_after)
                ],
                "buildSql",
            ),
            expected,
            "{build_new}"
        );
    }
}

const TAG_LIST: &str = "import { Tag } from \"./tag\";\n\nexport function List() {\n\treturn <Tag text=\"new\" />;\n}\n";
const TAG_TSCONFIG: &str = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\" } }\n";

/// `memo(` から `)` までを差し替えた `src/tag.tsx`。`{wrap}` に内側の関数を入れる。
fn memo_tag(import: &str, wrap: &str, props: &str) -> String {
    let function = format!("function Tag({props}) {{\n\treturn <span>{{text}}</span>;\n}}");
    format!(
        "{import}\n\nexport const Tag = {};\n",
        wrap.replace("{fn}", &function)
    )
}

/// `memo_tag` の変更前と変更後 (省略可能な `outlined` を足す) の組。
fn memo_tag_pair(import: &str, wrap: &str) -> (String, String) {
    (
        memo_tag(import, wrap, "{ text }: { text: string }"),
        memo_tag(
            import,
            wrap,
            "{\n\ttext,\n\toutlined = false,\n}: {\n\ttext: string;\n\toutlined?: boolean;\n}",
        ),
    )
}

/// 共通の base (package.json・tsconfig.json・利用側) に `extra` を足し、`src/tag.tsx` を
/// `old` → `new` に変えたときの `Tag` の区分。`changes` は同じ差分で変えるほかのファイル。
fn memo_bucket(
    package: &str,
    extra: &[(&str, &str)],
    old: &str,
    new: &str,
    changes: &[(&str, &str)],
) -> &'static str {
    memo_bucket_removing(package, extra, old, new, changes, &[])
}

/// [`memo_bucket`] に加えて、同じ差分で `removed` のファイルを消す。
fn memo_bucket_removing(
    package: &str,
    extra: &[(&str, &str)],
    old: &str,
    new: &str,
    changes: &[(&str, &str)],
    removed: &[&str],
) -> &'static str {
    let mut files = vec![
        ("package.json", package),
        ("tsconfig.json", TAG_TSCONFIG),
        ("src/tag.tsx", old),
        ("src/list.tsx", TAG_LIST),
    ];
    for (path, content) in extra {
        match files.iter_mut().find(|(existing, _)| existing == path) {
            Some(entry) => entry.1 = content,
            None => files.push((path, content)),
        }
    }
    let mut after = vec![("src/tag.tsx", new)];
    after.extend_from_slice(changes);
    bucket_after_removing(&files, &after, removed, "Tag")
}

/// React の `memo()` で包んだ関数部品の props の型リテラルに省略可能なプロパティを足すだけの変更は、
/// 包まない関数部品と同じく `optional_object_props` の互換 (Issue #266)。function 式・arrow 関数・
/// `memo as m`・括弧・同じ差分での利用側の変更でも同じ。
#[test]
fn ts_react_memo_optional_object_prop_is_compatible() {
    let import = "import { memo } from \"react\";";
    for (import, wrap) in [
        (import, "memo({fn})"),
        (import, "memo(({fn}))"),
        ("import { memo as m } from \"react\";", "m({fn})"),
        ("import React, { memo } from \"react\";", "(memo({fn},))"),
    ] {
        let (old, new) = memo_tag_pair(import, wrap);
        assert_eq!(
            memo_bucket(PRICE_PACKAGE, &[], &old, &new, &[]),
            "mod_compat",
            "{new}"
        );
    }
    let arrow = |props: &str| {
        format!(
            "import {{ memo }} from \"react\";\n\nexport const Tag = memo(({props}) => <span>{{text}}</span>);\n"
        )
    };
    assert_eq!(
        memo_bucket(
            PRICE_PACKAGE,
            &[],
            &arrow("{ text }: { text: string }"),
            &arrow("{ text, outlined = false }: { text: string; outlined?: boolean }"),
            &[],
        ),
        "mod_compat"
    );
    // 同じ差分で利用側を変えても、利用側の変更前が React の型を差し替えていなければ互換。
    let (old, new) = memo_tag_pair(import, "memo({fn})");
    assert_eq!(
        memo_bucket(
            PRICE_PACKAGE,
            &[],
            &old,
            &new,
            &[(
                "src/list.tsx",
                "import { Tag } from \"./tag\";\n\nexport function List() {\n\treturn <Tag text=\"new\" outlined />;\n}\n",
            )],
        ),
        "mod_compat"
    );
    // 指定子でない文字列やコメントの中のパス、中を指す相対の import、React 19 の型で `JSX.Element` を
    // 書くための `JSX` の import は、環境を差し替えない。
    assert_eq!(
        memo_bucket(
            PRICE_PACKAGE,
            &[
                (
                    "src/info.ts",
                    "export const label = \"ok\"; // ../../README.md\nexport const docs = \"../../../docs\";\n",
                ),
                (
                    "src/nested/view.ts",
                    "import { label } from \"../info\";\nexport const view = label;\n"
                ),
                (
                    "src/other.tsx",
                    "import type { JSX } from \"react\";\n\nexport function Other(): JSX.Element {\n\treturn <b />;\n}\n",
                ),
            ],
            &old,
            &new,
            &[],
        ),
        "mod_compat"
    );
}

/// `memo()` の証明に当たらないものは、従来どおり api.mod (対照)。react 以外の `memo`、比較関数・
/// 型引数を渡す呼び出し、`React.memo`、必須のプロパティの追加。
#[test]
fn ts_react_memo_unproven_wrappers_stay_modified() {
    let import = "import { memo } from \"react\";";
    let local_memo = [(
        "src/memo.ts",
        "export function memo<T>(component: T): T {\n\treturn component;\n}\n",
    )];
    for (import, wrap, extra) in [
        (
            "import { memo } from \"./memo\";",
            "memo({fn})",
            &local_memo[..],
        ),
        (import, "memo({fn}, (a, b) => a.text === b.text)", &[][..]),
        (import, "memo<{ text: string }>({fn})", &[][..]),
        (
            "import * as React from \"react\";",
            "React.memo({fn})",
            &[][..],
        ),
        (
            "import { memo } from \"react\";",
            "memo(forwardRef({fn}))",
            &[][..],
        ),
    ] {
        let (old, new) = memo_tag_pair(import, wrap);
        assert_eq!(
            memo_bucket(PRICE_PACKAGE, extra, &old, &new, &[]),
            "mod",
            "{new}"
        );
    }
    let old = memo_tag(import, "memo({fn})", "{ text }: { text: string }");
    let required = memo_tag(
        import,
        "memo({fn})",
        "{ text, outlined }: { text: string; outlined: boolean }",
    );
    assert_eq!(memo_bucket(PRICE_PACKAGE, &[], &old, &required, &[]), "mod");
}

/// `react` を registry の React と決められない・React の型を差し替えている・diff が環境を変える
/// ときは、`memo()` の結果の型を証明しない (対照)。
#[test]
fn ts_react_memo_requires_a_standard_react_environment() {
    let (old, new) = memo_tag_pair("import { memo } from \"react\";", "memo({fn})");
    let augmentation = "import \"react\";\n\ndeclare module \"react\" {\n\tfunction memo<T>(component: T, extra?: unknown): T;\n}\n";
    let no_react = "{ \"name\": \"repro\", \"private\": true, \"type\": \"module\" }\n";
    let paths = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"paths\": { \"react\": [\"./src/fake-react\"] } } }\n";
    let bare_module = "import \"react\";\n\nmodule \"react\" {\n\tfunction memo<P extends object>(f: (p: P) => null): (p: { [K in keyof P]-?: P[K] }) => null;\n}\n";
    let relative_augmentation = "import \"../node_modules/@types/react\";\n\ndeclare module \"../node_modules/@types/react\" {\n\tfunction memo<P extends object>(f: (p: P) => null): (p: { [K in keyof P]-?: P[K] }) => null;\n}\n";
    let type_roots = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"types\": [\"helpers\"], \"typeRoots\": [\"./types\"] } }\n";
    let type_entry = "{ \"types\": \"../../../shared/react-types.ts\" }\n";
    let paths_into_modules = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"paths\": { \"r\": [\"./node_modules/@types/react\"] } } }\n";
    let paths_outside = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"paths\": { \"@shared/*\": [\"../shared/*\"] } } }\n";
    let extends = "{ \"extends\": \"./configs/base.json\" }\n";
    let outside_import = "import \"../../shared/react-env\";\n";
    let strict = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"strict\": true } }\n";
    for (package, extra, changes) in [
        // ルートの package.json に react が無い
        (no_react, &[][..], &[][..]),
        // paths で react を差し替える
        (PRICE_PACKAGE, &[("tsconfig.json", paths)][..], &[][..]),
        // リポジトリのどこかで React の型を augmentation で差し替える
        (
            PRICE_PACKAGE,
            &[("src/react-memo.d.ts", augmentation)][..],
            &[][..],
        ),
        // 同じ差分で依存を変える
        (
            PRICE_PACKAGE,
            &[][..],
            &[(
                "package.json",
                "{ \"name\": \"repro\", \"private\": true, \"type\": \"module\", \"dependencies\": { \"react\": \"19.1.0\" }, \"devDependencies\": { \"@types/react\": \"19.1.0\" } }\n",
            )][..],
        ),
        // 変更前だけ React の型を差し替えていた
        (
            PRICE_PACKAGE,
            &[("src/react-memo.d.ts", augmentation)][..],
            &[("src/react-memo.d.ts", "export {};\n")][..],
        ),
        // `declare` を省いた `.d.ts` の module 宣言 (変更後にもある・変更前だけにある)
        (PRICE_PACKAGE, &[("src/env.d.ts", bare_module)][..], &[][..]),
        (
            PRICE_PACKAGE,
            &[("src/env.d.ts", bare_module)][..],
            &[("src/env.d.ts", "export {};\n")][..],
        ),
        // 相対パスで React の型の宣言ファイルを augmentation する
        (
            PRICE_PACKAGE,
            &[("src/env.ts", relative_augmentation)][..],
            &[][..],
        ),
        // typeRoots の型のパッケージが package.json で外のファイルを型の入口にする
        (
            PRICE_PACKAGE,
            &[
                ("tsconfig.json", type_roots),
                ("types/helpers/package.json", type_entry),
            ][..],
            &[][..],
        ),
        // paths が node_modules や解析するディレクトリの外を指す
        (
            PRICE_PACKAGE,
            &[("tsconfig.json", paths_into_modules)][..],
            &[][..],
        ),
        (
            PRICE_PACKAGE,
            &[("tsconfig.json", paths_outside)][..],
            &[][..],
        ),
        // extends で読む、tsconfig の名前でない設定を同じ差分で変える
        (
            PRICE_PACKAGE,
            &[
                ("tsconfig.json", extends),
                ("configs/base.json", TAG_TSCONFIG),
            ][..],
            &[("configs/base.json", strict)][..],
        ),
        // 解析するディレクトリの外のファイルを import で取り込む (変更後にもある・変更前だけにある)
        (
            PRICE_PACKAGE,
            &[("src/env.ts", outside_import)][..],
            &[][..],
        ),
        (
            PRICE_PACKAGE,
            &[("src/env.ts", outside_import)][..],
            &[("src/env.ts", "export {};\n")][..],
        ),
    ] {
        assert_eq!(
            memo_bucket(package, extra, &old, &new, changes),
            "mod",
            "{package} {extra:?} {changes:?}"
        );
    }
}

/// `memo()` で包んだ関数部品の props が参照する同じファイルの型の変更は、包まない関数部品と同じく
/// 部品の型の変更として扱う (Issue #266 で、依存の検査が包んだ関数を見るようにした)。
#[test]
fn ts_react_memo_props_type_dependencies_are_tracked() {
    let tag = |members: &str| {
        format!(
            "import {{ memo }} from \"react\";\n\ntype TagProps = {{ {members} }};\n\nexport const Tag = memo(function Tag({{ text }}: TagProps) {{\n\treturn <span>{{text}}</span>;\n}});\n"
        )
    };
    assert_eq!(
        memo_bucket(
            PRICE_PACKAGE,
            &[],
            &tag("text: string"),
            &tag("text: string; size: number"),
            &[],
        ),
        "mod"
    );
}

/// 変更前に `react` の解決先だったかもしれないファイル (`baseUrl` の下の `react.ts`) を同じ差分で
/// 消したときは、変更前の `memo` を React のものと決められないので証明しない (対照)。
#[test]
fn ts_react_memo_rejects_a_removed_resolution_candidate() {
    let (old, new) = memo_tag_pair("import { memo } from \"react\";", "memo({fn})");
    let base_url = "{ \"compilerOptions\": { \"jsx\": \"react-jsx\", \"baseUrl\": \"./src\" } }\n";
    let fake = "export function memo<T>(component: T): T {\n\treturn component;\n}\n";
    assert_eq!(
        memo_bucket_removing(
            PRICE_PACKAGE,
            &[("tsconfig.json", base_url), ("src/react.ts", fake)],
            &old,
            &new,
            &[],
            &["src/react.ts"],
        ),
        "mod"
    );
    // 対照: 解決先の候補でないファイルを消すだけなら証明する。
    assert_eq!(
        memo_bucket_removing(
            PRICE_PACKAGE,
            &[("tsconfig.json", base_url), ("src/unused.ts", fake)],
            &old,
            &new,
            &[],
            &["src/unused.ts"],
        ),
        "mod_compat"
    );
}

/// `memo()` の内側の関数でも、props が参照する同じファイルの型を同じ差分で変えたとき・2 つ目の
/// 引数を足したときは、省略可能なプロパティの追加として証明しない (対照)。
#[test]
fn ts_react_memo_keeps_other_changes_modified() {
    let tag = |alias: &str, props: &str| {
        format!(
            "import {{ memo }} from \"react\";\n\ntype Text = {alias};\n\nexport const Tag = memo(function Tag({props}) {{\n\treturn <span>{{text}}</span>;\n}});\n"
        )
    };
    assert_eq!(
        memo_bucket(
            PRICE_PACKAGE,
            &[],
            &tag("string", "{ text }: { text: Text }"),
            &tag(
                "number",
                "{ text, outlined = false }: { text: Text; outlined?: boolean }"
            ),
            &[],
        ),
        "mod"
    );
    let arrow = |params: &str| {
        format!(
            "import {{ memo }} from \"react\";\n\nexport const Tag = memo(({params}) => <span>{{text}}</span>);\n"
        )
    };
    assert_eq!(
        memo_bucket(
            PRICE_PACKAGE,
            &[],
            &arrow("{ text }: { text: string }"),
            &arrow("{ text }: { text: string }, legacy?: unknown"),
            &[],
        ),
        "mod"
    );
}

/// `react` と `@types/react` をワークスペースのパッケージの package.json にだけ書くモノレポでも、
/// そのパッケージの部品は `optional_object_props` の互換 (Issue #280)。宣言の無いほかの
/// ワークスペースのパッケージの部品は、hoisting で解決されても宣言からは決められないので、従来
/// どおり証明しない。
#[test]
fn ts_react_memo_in_a_workspace_package_is_compatible() {
    let (old, new) = memo_tag_pair("import { memo } from \"react\";", "memo({fn})");
    let root = "{ \"name\": \"mono\", \"private\": true, \"workspaces\": [\"packages/*\"] }\n";
    let ui = "{ \"name\": \"@example/ui\", \"private\": true, \"type\": \"module\", \"dependencies\": { \"react\": \"19.0.0\" }, \"devDependencies\": { \"@types/react\": \"19.0.0\" } }\n";
    let other = "{ \"name\": \"@example/other\", \"private\": true, \"type\": \"module\" }\n";
    for (package, expected) in [("ui", "mod_compat"), ("other", "mod")] {
        let tag = format!("packages/{package}/src/tag.tsx");
        let list = format!("packages/{package}/src/list.tsx");
        let tsconfig = format!("packages/{package}/tsconfig.json");
        let files = [
            ("package.json", root),
            ("packages/ui/package.json", ui),
            ("packages/other/package.json", other),
            (tsconfig.as_str(), TAG_TSCONFIG),
            (tag.as_str(), old.as_str()),
            (list.as_str(), TAG_LIST),
        ];
        assert_eq!(
            bucket_after(&files, &[(tag.as_str(), new.as_str())], "Tag"),
            expected,
            "{package}"
        );
    }
}
