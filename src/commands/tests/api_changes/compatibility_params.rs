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
