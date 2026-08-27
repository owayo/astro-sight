//! TypeScript / TSX の API 差分検出テスト。

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

/// TSX 関数コンポーネントの destructured props に optional prop を追加するだけの
/// React 後方互換変更は api.mod に出してはならない (Issue
/// 引数なし TS/TSX 関数に、`= {}` default 付きの destructured props を追加する
/// 後方互換変更は api.mod に出してはならない (Issue
/// 2026-05-28-meet-virtual-you-frontend-modernize 対応)。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_default_value_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function TemplateManager() {\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("TemplateManager.tsx"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/TemplateManager.tsx"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // 引数なし → destructured props + `= {}` default 付き (省略可能)
    let after = concat!(
        "interface TemplateManagerProps {\n",
        "  onSaved?: (message: string) => void;\n",
        "}\n",
        "export function TemplateManager({ onSaved }: TemplateManagerProps = {}) {\n",
        "  onSaved?.(\"ok\");\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("TemplateManager.tsx"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/TemplateManager.tsx".to_string(),
        new_path: "src/TemplateManager.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"TemplateManager"),
        "default `= {{}}` 付きの destructured props 追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) するが
/// 型注釈の `interface` が同一ファイル内で全 optional な場合、省略可能と
/// 判定して api.mod に出してはならない。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_all_optional_interface_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function SpeakerNameSetting() {\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("SpeakerNameSetting.tsx"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/SpeakerNameSetting.tsx"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // 引数なし → destructured props + 同一ファイル内 interface (全 optional)
    let after = concat!(
        "interface SpeakerNameSettingProps {\n",
        "  onSaved?: (message: string) => void;\n",
        "}\n",
        "export function SpeakerNameSetting({ onSaved }: SpeakerNameSettingProps) {\n",
        "  onSaved?.(\"ok\");\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("SpeakerNameSetting.tsx"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/SpeakerNameSetting.tsx".to_string(),
        new_path: "src/SpeakerNameSetting.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"SpeakerNameSetting"),
        "同一ファイル内 interface が全 optional なら destructured props 追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) し、型注釈の
/// inline object type に required field を含む場合は破壊的変更として
/// api.mod に残すべき (副作用回帰防止)。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_required_field_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // 引数なし → required field を含む inline object type の destructured props
    let after = concat!(
        "export function widget({ name }: { name: string }) {\n",
        "  return name;\n",
        "}\n",
        "export function caller() { return widget({ name: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "required field を持つ inline object type の destructured props 追加は api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) し、型注釈が
/// import 型 (同一ファイル内に declaration なし) の場合は省略可能と断定できない
/// ため api.mod に残すべき (副作用回帰防止)。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_imported_type_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // 引数なし → 同一ファイルに declaration がない type identifier
    let after = concat!(
        "import type { WidgetProps } from './props';\n",
        "export function widget({ name }: WidgetProps) {\n",
        "  return name;\n",
        "}\n",
        "export function caller() { return widget({ name: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "import 型 (同ファイル内 declaration なし) の destructured props 追加は省略可能と断定できないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// TS 関数 destructured params の型注釈 (inline object type) で optional field の
/// 型を変更した場合 (`{ x?: string }` → `{ x?: number }`) は呼び出し側に見える
/// 型契約変更なので api.mod に残すべき。「省略可能 destructured を `()` と
/// 同一視する」過剰正規化を防ぐ codex 指摘 1 への回帰防止。
#[test]
fn detect_api_changes_tsx_optional_field_type_change_in_destructured_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function foo({ x }: { x?: string }): string {\n",
        "  return x ?? \"a\";\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    fs::write(
        src_dir.join("caller.ts"),
        "import { foo } from './foo';\nexport const x = foo({ x: 'a' });\n",
    )
    .expect("write caller");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/caller.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // optional field の型変更 (string → number)
    let after = concat!(
        "export function foo({ x }: { x?: number }): string {\n",
        "  return String(x ?? 0);\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "optional field の型変更は呼び出し側型契約変更なので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// `interface Props extends Base { ... }` で body のフィールドが全 optional でも、
/// base interface が required field を持つ可能性があるため省略可能扱いしない
/// (codex 指摘 2 への回帰防止)。
#[test]
fn detect_api_changes_tsx_interface_with_extends_clause_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // interface に extends を付けて props を追加 (body は optional だが base が不明)
    let after = concat!(
        "interface BaseProps {\n",
        "  required: string;\n",
        "}\n",
        "interface WidgetProps extends BaseProps {\n",
        "  optional?: number;\n",
        "}\n",
        "export function widget({ optional }: WidgetProps) {\n",
        "  return optional;\n",
        "}\n",
        "export function caller() { return widget({ required: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "extends 持ち interface は base 側の required field を否定できないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 同名 interface declaration merge で、片方が required field を含む場合、
/// 全体としては省略可能ではないので api.mod に残すべき (codex 指摘 3 への
/// 回帰防止)。
#[test]
fn detect_api_changes_tsx_interface_merge_with_required_field_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // 同名 interface 宣言が 2 つあり、片方は optional のみ、もう片方は required あり
    let after = concat!(
        "interface WidgetProps {\n",
        "  optional?: number;\n",
        "}\n",
        "interface WidgetProps {\n",
        "  required: string;\n",
        "}\n",
        "export function widget({ optional }: WidgetProps) {\n",
        "  return optional;\n",
        "}\n",
        "export function caller() { return widget({ required: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "同名 interface merge で required field があれば省略可能ではないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// `"name?": string` のような string property name の `?` を optional マーカーと
/// 誤判定しないこと (codex 指摘 4 への回帰防止)。required field を含む型注釈
/// なので api.mod に残るべき。
#[test]
fn detect_api_changes_tsx_string_property_name_with_question_mark_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // string property name の中に `?` を含む required field を持つ inline object type
    let after = concat!(
        "export function widget(props: { \"name?\": string }) {\n",
        "  return props[\"name?\"];\n",
        "}\n",
        "export function caller() { return widget({ \"name?\": \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "string property name `\"name?\"` の `?` は optional マーカーではなく required field のはず。api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 旧関数が型注釈内に同名の call signature を含む場合でも、AST で旧 parameters を
/// 検査して誤判定しないこと (codex 指摘 5 への回帰防止)。旧 sig 文字列に
/// `foo()` という部分文字列が含まれても、実際の関数 foo は引数を取るので
/// api.mod に残るべき。
#[test]
fn detect_api_changes_tsx_old_signature_contains_inline_call_signature_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    // 旧: 引数あり (引数の型注釈に foo() という inline call signature を含む)
    let before = concat!(
        "export function foo(arg: { foo(): void }) {\n",
        "  arg.foo();\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { foo } from './foo';\nexport const x = foo({ foo: () => {} });\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/user.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // 新: 引数を destructured + 型注釈に optional のみの inline object に変更
    let after = concat!(
        "export function foo({ x }: { x?: string }) {\n",
        "  return x ?? \"a\";\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "旧関数が引数を取る場合は (型注釈内 call signature があっても) api.mod に残すべき。got: {mod_names:?}"
    );
}

/// ネストしたローカル同名関数を拾わないこと (codex 指摘 6 への回帰防止)。
/// 変更対象の exported 関数 widget は required props だが、関数内ネストに
/// 同名 widget があり optional だとしても、トップレベル限定の判定で
/// api.mod に残すべき。
#[test]
fn detect_api_changes_tsx_nested_local_function_does_not_override_top_level_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // 新: トップレベル widget は required props、ネスト widget は optional のみ
    let after = concat!(
        "export function widget({ required }: { required: string }) {\n",
        "  function widget({ optional }: { optional?: string }) {\n",
        "    return optional;\n",
        "  }\n",
        "  return widget({});\n",
        "}\n",
        "export function caller() { return widget({ required: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "トップレベル widget は required props なので、ネスト同名関数に惑わされず api.mod に残すべき。got: {mod_names:?}"
    );
}

/// TSX 関数コンポーネントの destructured props に optional prop を追加するだけの
/// React 後方互換変更は api.mod に出してはならない (Issue
/// 2026-05-28-api-mod-optional-props-additive 対応)。
#[test]
fn detect_api_changes_tsx_destructured_props_optional_addition_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export interface Props { templates: string[]; onSelect: (s: string) => void; className?: string }\n",
        "export function PromptTemplateSelector({ templates, onSelect, className = \"\" }: Props) {\n",
        "  return templates;\n",
        "}\n"
    );
    fs::write(src_dir.join("Selector.tsx"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/Selector.tsx"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // interface に optional prop を追加し、関数の destructure 受け取りも追加。
    // 型注釈 `: Props` 自体は不変。
    let after = concat!(
        "export interface Props { templates: string[]; onSelect: (s: string) => void; className?: string; useExistingContent?: boolean; onChange?: (v: boolean) => void }\n",
        "export function PromptTemplateSelector({ templates, onSelect, className = \"\", useExistingContent = false, onChange }: Props) {\n",
        "  return templates;\n",
        "}\n"
    );
    fs::write(src_dir.join("Selector.tsx"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/Selector.tsx".to_string(),
        new_path: "src/Selector.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"PromptTemplateSelector"),
        "TSX destructured params の optional 受け取り追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// TS 関数の destructured params のデフォルト値変更は signature 不変として扱う
/// (caller-visible な型契約ではなく binding 時の挙動変更)。
#[test]
fn detect_api_changes_typescript_destructured_default_value_change_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export interface Opts { x?: number }\n",
        "export function foo({ x = 0 }: Opts) {\n",
        "  return x;\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    let after = concat!(
        "export interface Opts { x?: number }\n",
        "export function foo({ x = 42 }: Opts) {\n",
        "  return x;\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"foo"),
        "destructured params の default value 変更は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// TS 関数の positional 引数追加は destructure ではなく直接の呼び出し契約変更なので
/// api.mod に残す (destructure normalize の副作用回帰防止)。
#[test]
fn detect_api_changes_typescript_positional_param_added_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function foo(a: number): number {\n",
        "  return a;\n",
        "}\n",
        "export function bar() { return foo(1); }\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    // 他ファイルからの cross-file 参照を作って closed-in-diff で抑制されないようにする。
    fs::write(
        src_dir.join("caller.ts"),
        "import { foo } from './foo';\nexport const x = foo(1);\n",
    )
    .expect("write caller");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/caller.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    let after = concat!(
        "export function foo(a: number, b: number): number {\n",
        "  return a + b;\n",
        "}\n",
        "export function bar() { return foo(1, 2); }\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "positional 引数追加は destructure ではないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// TS 関数 destructured params の **inline object type** 注釈変更は signature 変更として
/// 残す (型注釈側は呼び出し側に見える契約)。destructure normalize が type_annotation
/// に踏み込まないことの回帰防止。
#[test]
fn detect_api_changes_typescript_inline_object_type_change_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function foo({ x }: { x: string }): string {\n",
        "  return x;\n",
        "}\n",
        "export function bar() { return foo({ x: 'a' }); }\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    fs::write(
        src_dir.join("caller.ts"),
        "import { foo } from './foo';\nexport const x = foo({ x: 'a' });\n",
    )
    .expect("write caller");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/caller.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // inline object type に required な y フィールドを追加 (breaking)
    let after = concat!(
        "export function foo({ x, y }: { x: string; y: number }): string {\n",
        "  return x + y;\n",
        "}\n",
        "export function bar() { return foo({ x: 'a', y: 1 }); }\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "inline object type 注釈の構造変更は api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 新規 export 型が同一ファイル内の型注釈からのみ参照される場合、api.add に載せつつ
/// 同一ファイル内の実利用参照数 (`refs_internal`) を添える。
///
/// api.add の抽出条件は「同一ファイル内の**呼び出し**参照が無い」+「同一 diff の他ファイル
/// からの実利用参照が無い」の合成で、TS の型注釈は呼び出しではないため条件に現れない。
/// 参照数を添えないと `refs` を別途実行するまで「同一ファイル内には参照がある」と分からず、
/// トリアージが「完全に未参照 = デッドコード」と読み違える
/// (Issue 2026-08-04-review-add-scope-naming)
#[test]
fn detect_api_changes_ts_new_export_type_counts_internal_annotation_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[
            (
                "module.ts",
                "export interface ShareRow {\n  id: string;\n}\n\nexport async function fetchShare(id: string): Promise<ShareRow[]> {\n  return [{ id }];\n}\n",
            ),
            (
                "caller.ts",
                "import { fetchShare } from \"./module\";\n\nexport async function render(id: string): Promise<number> {\n  const rows = await fetchShare(id);\n  return rows.length;\n}\n",
            ),
        ],
        "initial",
    );

    // 新: TypeObservation を追加し、同一ファイル内の型注釈 2 箇所 (ShareResult の
    // フィールド型 / const の型注釈) からのみ参照する。ShareResult は caller.ts が
    // import するため api.add には載らない (同一 diff 内の他ファイル参照あり)。
    fs::write(
        repo.join("module.ts"),
        "export interface ShareRow {\n  id: string;\n}\n\nexport interface TypeObservation {\n  at: string;\n}\n\nexport interface ShareResult {\n  rows: ShareRow[];\n  recentObservation: TypeObservation | null;\n}\n\nexport async function fetchShare(id: string): Promise<ShareResult> {\n  const recentObservation: TypeObservation | null = null;\n  return { rows: [{ id }], recentObservation };\n}\n",
    )
    .expect("write module.ts");
    fs::write(
        repo.join("caller.ts"),
        "import { fetchShare, type ShareResult } from \"./module\";\n\nexport async function render(id: string): Promise<number> {\n  const result: ShareResult = await fetchShare(id);\n  return result.rows.length;\n}\n",
    )
    .expect("write caller.ts");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "module.ts".to_string(),
            new_path: "module.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 7,
                new_start: 1,
                new_count: 17,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "caller.ts".to_string(),
            new_path: "caller.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
    ];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let observation = api
        .added
        .iter()
        .find(|s| s.name == "TypeObservation")
        .unwrap_or_else(|| panic!("TypeObservation は api.add に載るべき: {:?}", api.added));
    assert_eq!(
        observation.refs_internal, 2,
        "同一ファイル内の型注釈参照 2 件を数えるべき: {observation:?}"
    );
    assert!(
        !api.added.iter().any(|s| s.name == "ShareResult"),
        "他ファイルから import される ShareResult は api.add に載らない: {:?}",
        api.added
    );
}

/// TS で export 関数と同名のローカル関数が別ファイルにあっても、shadow 解決で
/// ローカル呼び出しを除外し、対象 caller (複数行呼び出しの引数内のみ変更) が追随済みなら
/// closed-in-diff に降格する (Issue 2026-07-12-api-mod-same-diff-informational の完全再現)。
#[test]
fn detect_api_changes_ts_shadowed_local_fn_and_multiline_call_is_closed_in_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n    audio: boolean;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({\n        fps: 30,\n        audio: true,\n    });\n}\n",
            ),
        ],
        "base",
    );
    // capture.ts: シグネチャに cursor を追加 / content.ts: 複数行呼び出しの引数内にだけ
    // 追随行を追加 (識別子行 `startRecording({` 自体は未変更) / tap.ts は触らない。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    audio: boolean;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({\n        fps: 30,\n        audio: true,\n        cursor: true,\n    });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 4,
                new_start: 4,
                new_count: 5,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "同名ローカル関数の shadow 除外 + 複数行呼び出しの引数内変更で closed に降格すべき。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
    assert!(
        !api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "closed-in-diff は blocking な modified に残さない"
    );
}

/// 呼び出し式は無変更でも、渡している共有 `const` の定義側に同一 diff 内で当該必須
/// プロパティが追加されていれば closed-in-diff (informational) に降格する。
///
/// 旧実装は「参照行 / enclosing call_expression の行範囲が実変更行と交差するか」でしか
/// 見ておらず、`buildSql(SHARED_DEPS)` のように呼び出し式そのものが無変更のケースを
/// 未更新 caller として blocking にしていた。
#[test]
fn detect_api_changes_ts_shared_const_arg_updated_in_diff_is_closed_in_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api = ts_shared_const_arg_api_changes(
        dir.path(),
        TS_SHARED_CONST_CALLER_BEFORE,
        "import { buildSql } from \"./build\";\n\nconst SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
    );
    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("buildSql")),
        "共有 const の定義側が同一 diff で更新済みなら closed に降格すべき。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
    assert!(
        !api.modified.iter().any(|m| m.name.ends_with("buildSql")),
        "closed-in-diff は blocking な modified に残さない"
    );
}

/// 共有 `const` の定義が更新されていなければ従来どおり blocking (本当に未更新の caller)。
#[test]
fn detect_api_changes_ts_shared_const_arg_not_updated_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 呼び出し側は無関係な行だけ変更し、SHARED_DEPS に groups を足さない。
    let api = ts_shared_const_arg_api_changes(
        dir.path(),
        TS_SHARED_CONST_CALLER_BEFORE,
        "import { buildSql } from \"./build\";\n\nconst SHARED_DEPS = {\n\tevents: \"e2\",\n\tusers: \"u\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
    );
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("buildSql")),
        "必須プロパティが共有 const に足されていなければ blocking を維持すべき。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// 実引数の直前にコメントがあっても分類は変わらない。
///
/// `comment` は `arguments` の named child なので、除外せずに index すると
/// `foo(/* c */ A, B)` で実引数の位置が仮引数の位置から 1 つずれ、別の引数を
/// 共有 const として検査してしまう。結果として `/* c */` の有無だけで
/// blocking (`api.mod`) と informational (`mod_closed`) が反転していた。
#[test]
fn detect_api_changes_ts_comment_in_call_arguments_does_not_change_classification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api = ts_shared_const_arg_api_changes(
        dir.path(),
        TS_SHARED_CONST_CALLER_BEFORE,
        "import { buildSql } from \"./build\";\n\nconst SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n};\n\nexport function run(): string {\n\treturn buildSql(/* deps */ SHARED_DEPS);\n}\n",
    );
    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("buildSql")),
        "引数直前のコメントは分類を変えてはならない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
    assert!(
        !api.modified.iter().any(|m| m.name.ends_with("buildSql")),
        "コメント付き呼び出しが blocking な modified に残ってはならない"
    );
}

/// リポジトリ内に呼び出し参照が 1 件も無い exported 関数のシグネチャ変更は、
/// **blocking な api.mod のまま**で `no_resolved_internal_callers` フラグだけ立てる。
///
/// 参照 0 件は「未使用」ではなく「静的に解決できた内部参照が 0 件」でしかなく、外部リポジトリ
/// からの利用・動的呼び出し・文字列参照と区別できない (公開ライブラリではむしろ最重要の
/// 破壊的変更)。カテゴリ分離も blocking 解除もせず、トリアージが「呼び出し側を探す」段階を
/// 省けるようにするだけ (Issue 2026-08-05-api-mod-callers-updated-indirectly パターン B)。
#[test]
fn detect_api_changes_ts_modified_without_callers_is_flagged_but_still_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/actions.ts",
            "export function submitToggle(id: string): void {\n\tconsole.log(id);\n}\n",
        )],
        "base",
    );
    // 呼び出し側はリポジトリ内に 1 件も無い (UI 側は過去に削除済み) 状態で引数を追加する。
    fs::write(
        repo.join("src/actions.ts"),
        "export function submitToggle(id: string, force: boolean): void {\n\tconsole.log(id, force);\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/actions.ts".to_string(),
        new_path: "src/actions.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let change = api
        .modified
        .iter()
        .find(|m| m.name.ends_with("submitToggle"))
        .unwrap_or_else(|| {
            panic!(
                "呼び出し側 0 件でも api.mod は blocking のまま維持すべき。mod={:?} mod_closed={:?}",
                api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
                api.modified_closed_in_diff
                    .iter()
                    .map(|m| &m.name)
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        change.no_resolved_internal_callers,
        "解決できた呼び出し参照が 0 件ならフラグを立てるべき: {change:?}"
    );
}

/// 呼び出し参照があるシンボルにはフラグを立てない (フラグが常時 true にならないことの固定)。
#[test]
fn detect_api_changes_ts_modified_with_callers_is_not_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/actions.ts",
                "export function submitToggle(id: string): void {\n\tconsole.log(id);\n}\n",
            ),
            (
                "src/ui.ts",
                "import { submitToggle } from \"./actions\";\n\nexport function onClick(): void {\n\tsubmitToggle(\"a\");\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/actions.ts"),
        "export function submitToggle(id: string, force: boolean): void {\n\tconsole.log(id, force);\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/actions.ts".to_string(),
        new_path: "src/actions.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let change = api
        .modified
        .iter()
        .find(|m| m.name.ends_with("submitToggle"))
        .expect("未更新 caller があるので blocking な api.mod に残る");
    assert!(
        !change.no_resolved_internal_callers,
        "呼び出し参照があるシンボルにはフラグを立てない: {change:?}"
    );
}

/// 引数が `const` ではなく `let` の場合は再代入されうるため降格しない (fail-closed)。
#[test]
fn detect_api_changes_ts_shared_let_arg_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api = ts_shared_const_arg_api_changes(
        dir.path(),
        "import { buildSql } from \"./build\";\n\nlet SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        "import { buildSql } from \"./build\";\n\nlet SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
    );
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("buildSql")),
        "let 束縛は再代入されうるので降格しない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// 同名 binding が他にもある (shadow の可能性) 場合は降格しない (fail-closed)。
/// 呼び出し位置で実際に渡る値を静的に決められないため。
#[test]
fn detect_api_changes_ts_shadowed_const_arg_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api = ts_shared_const_arg_api_changes(
        dir.path(),
        "import { buildSql } from \"./build\";\n\nconst SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n};\n\nexport function run(SHARED_DEPS: { events: string; users: string }): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        "import { buildSql } from \"./build\";\n\nconst SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n};\n\nexport function run(SHARED_DEPS: { events: string; users: string }): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
    );
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("buildSql")),
        "同名 binding が複数あれば shadow の可能性があり降格しない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// 各種 shadow 構文で降格しないこと (binding 検出漏れ = shadow 見逃し = fail-open の回帰防止)。
///
/// identifier の親 field 名だけで binding を判定していると、bare arrow パラメータ・catch
/// パラメータ・renamed destructuring・配列/rest destructuring を取りこぼし、実際に渡るのは
/// 更新されていないローカル値なのにトップレベル const を見て降格してしまう。
/// いずれのケースもトップレベルに「更新済みの `const SHARED_DEPS`」を置いたうえで、
/// 呼び出し位置ではそれが shadow されている形にしてある。
#[test]
fn detect_api_changes_ts_shadowing_binding_forms_stay_modified() {
    // (ケース名, 呼び出し側ファイルの本体。SHARED_DEPS を shadow する binding を含む)
    // 実引数はいずれも**裸の identifier** にする (`X as never` のような cast を挟むと
    // 「bare identifier ではない」という別の理由で不成立になり、shadow ガードを検証できない)。
    let cases: [(&str, &str); 11] = [
        (
            "bare arrow parameter",
            "export const run = (SHARED_DEPS) => buildSql(SHARED_DEPS);\n",
        ),
        (
            "catch parameter",
            "export function run(): string {\n\ttry {\n\t\treturn \"\";\n\t} catch (SHARED_DEPS) {\n\t\treturn buildSql(SHARED_DEPS);\n\t}\n}\n",
        ),
        (
            "renamed destructuring",
            "export function run(input): string {\n\tconst { deps: SHARED_DEPS } = input;\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        (
            "array destructuring",
            "export function run(input): string {\n\tconst [SHARED_DEPS] = input;\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        (
            "rest parameter",
            "export function run(...SHARED_DEPS): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        // `using X = ...` は tree-sitter-typescript では匿名 `using` トークン付きの
        // assignment_expression になり専用ノードが無い (実ノードを to_sexp で確認済み)。
        (
            "using declaration",
            "export function run(): string {\n\tusing SHARED_DEPS = acquire();\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        // `abstract class` は `abstract_class_declaration` (name は type_identifier)。
        (
            "abstract class",
            "export function run(): string {\n\tabstract class SHARED_DEPS {}\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        // TS の import alias は `import_alias` で `name` field を持たず先頭 named child が
        // ローカル名。ブロック内の無関係な const だけを一意 binding と誤認しないこと。
        (
            "import alias",
            "namespace Legacy {\n\texport const Value = { events: \"e\", users: \"u\" };\n}\nimport SHARED_DEPS = Legacy.Value;\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        // `declare function X()` は `function_signature` (kind が `_declaration` で終わらない)。
        (
            "declare function signature",
            "declare function SHARED_DEPS(a: number): void;\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        // `import X = require("...")` は `import_require_clause` (name field なし)。
        (
            "import require clause",
            "import SHARED_DEPS = require(\"./deps\");\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
        // `namespace X.Legacy {}` の name は `nested_identifier`。ローカルに入るのは左端の X。
        (
            "nested namespace name",
            "namespace SHARED_DEPS.Legacy {\n\texport const v = 1;\n}\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        ),
    ];
    for (label, body) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        // 変更前後で共通の「トップレベル const」部分。after 側では groups を追加済みにして、
        // shadow を見逃したら降格してしまう状況を作る。
        let before = format!(
            "import {{ buildSql }} from \"./build\";\n\nconst SHARED_DEPS = {{\n\tevents: \"e\",\n\tusers: \"u\",\n}};\n\nexport const unused = SHARED_DEPS;\n\n{body}"
        );
        let after = format!(
            "import {{ buildSql }} from \"./build\";\n\nconst SHARED_DEPS = {{\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n}};\n\nexport const unused = SHARED_DEPS;\n\n{body}"
        );
        let api = ts_shared_const_arg_api_changes(dir.path(), &before, &after);
        assert!(
            api.modified.iter().any(|m| m.name.ends_with("buildSql")),
            "{label}: shadow された binding は静的に値を決められないので降格しない。mod={:?} mod_closed={:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
            api.modified_closed_in_diff
                .iter()
                .map(|m| &m.name)
                .collect::<Vec<_>>()
        );
    }

    // 対照ケース: 上と同じ骨格で shadow だけ取り除く (パラメータ名を変える) と降格する。
    // これが無いと「そもそも降格し得ない形だから modified のまま」でも上の assert が通り、
    // shadow ガードのテストとして成立しない。
    let dir = tempfile::tempdir().expect("tempdir");
    let control_body = "export const run = (other) => buildSql(SHARED_DEPS) + other;\n";
    let control_before = format!(
        "import {{ buildSql }} from \"./build\";\n\nconst SHARED_DEPS = {{\n\tevents: \"e\",\n\tusers: \"u\",\n}};\n\nexport const unused = SHARED_DEPS;\n\n{control_body}"
    );
    let control_after = format!(
        "import {{ buildSql }} from \"./build\";\n\nconst SHARED_DEPS = {{\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n}};\n\nexport const unused = SHARED_DEPS;\n\n{control_body}"
    );
    let api = ts_shared_const_arg_api_changes(dir.path(), &control_before, &control_after);
    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("buildSql")),
        "対照ケース (shadow なし) は降格するはず。降格しないなら上の shadow テストが\
         shadow ガードを検証できていない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// パラメータの **default 値**に共有 const が現れても、それは binding ではなく参照なので
/// shadow とみなさず降格する (`pattern_binds_name` が `assignment_pattern` の右辺まで辿ると
/// `function run(other = SHARED_DEPS)` を parameter binding と誤認し、降格できるはずのケースを
/// blocking に残す false positive になる)。
#[test]
fn detect_api_changes_ts_const_arg_in_default_value_is_not_a_shadow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = "export function run(other = SHARED_DEPS): string {\n\treturn buildSql(SHARED_DEPS) + String(other);\n}\n";
    let before = format!(
        "import {{ buildSql }} from \"./build\";\n\nconst SHARED_DEPS = {{\n\tevents: \"e\",\n\tusers: \"u\",\n}};\n\n{body}"
    );
    let after = format!(
        "import {{ buildSql }} from \"./build\";\n\nconst SHARED_DEPS = {{\n\tevents: \"e\",\n\tusers: \"u\",\n\tgroups: \"g\",\n}};\n\n{body}"
    );
    let api = ts_shared_const_arg_api_changes(dir.path(), &before, &after);
    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("buildSql")),
        "default 値の参照は binding ではないので shadow 扱いせず降格すべき。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// spread を含む object literal はキー集合を静的に確定できないため降格しない (fail-closed)。
#[test]
fn detect_api_changes_ts_spread_const_arg_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let api = ts_shared_const_arg_api_changes(
        dir.path(),
        "import { buildSql } from \"./build\";\n\nconst BASE = { users: \"u\" };\nconst SHARED_DEPS = {\n\t...BASE,\n\tevents: \"e\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
        "import { buildSql } from \"./build\";\n\nconst BASE = { users: \"u\" };\nconst SHARED_DEPS = {\n\t...BASE,\n\tevents: \"e\",\n\tgroups: \"g\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n",
    );
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("buildSql")),
        "spread 混じりの object はキー集合を確定できないため降格しない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// 同名ローカル関数の shadow があっても、対象 caller が diff 外なら従来どおり blocking。
#[test]
fn detect_api_changes_ts_shadowed_local_fn_with_caller_outside_diff_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
        ],
        "base",
    );
    // capture.ts のみ変更。content.ts (対象 caller) は未更新かつ diff 外。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/capture.ts".to_string(),
        new_path: "src/lib/capture.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "対象 caller が diff 外なら blocking な modified のまま。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// switch body 内の block-level 同名関数は外側の呼び出しを shadow しない: 更新済み caller と
/// 「switch 内同名関数を持つファイルの未更新 caller」が併存する場合、後者を shadow 除外せず
/// blocking を維持する (codex レビュー指摘の switch_body scope 対応)。
#[test]
fn detect_api_changes_ts_switch_scoped_fn_does_not_shadow_unupdated_caller() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            // switch 内に block-level 同名関数 + その外に対象 API への未更新呼び出し
            (
                "src/tap.ts",
                "import { startRecording } from \"./lib/capture\";\nexport function caller(x: number) {\n    switch (x) {\n        case 1:\n            function startRecording() {}\n    }\n    return startRecording({ fps: 30 });\n}\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
        ],
        "base",
    );
    // 対象 API 変更 + content.ts (更新済み caller) のみ追随。tap.ts の呼び出しは未更新・diff 外。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30, cursor: true });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 1,
                new_start: 4,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "switch 内関数は shadow にならず、未更新 caller (tap.ts) がある限り blocking 維持。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// for-of の loop 変数は for scope の binding: 同名 loop 変数経由の呼び出し (中身は alias
/// import した対象 API かもしれない) を、外側の同名ローカル関数への束縛と誤解決して shadow
/// 除外しない — 未追随の可能性がある限り blocking を維持する (for ヘッダ binding 対応)。
#[test]
fn detect_api_changes_ts_for_of_loop_variable_does_not_shadow_unupdated_caller() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            // ローカル同名関数 + alias import した対象 API を loop 変数 (同名) 経由で呼ぶ。
            // loop 変数への呼び出しはローカル関数に束縛されない (for scope の binding)。
            (
                "src/tap.ts",
                "import { startRecording as rec } from \"./lib/capture\";\nfunction startRecording() {}\nexport function caller() {\n    for (const startRecording of [rec]) {\n        startRecording({ fps: 30 });\n    }\n    return startRecording;\n}\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
        ],
        "base",
    );
    // 対象 API 変更 + content.ts (更新済み caller) のみ追随。tap.ts の loop 変数呼び出しは
    // 未更新・diff 外のまま。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30, cursor: true });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 1,
                new_start: 4,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "loop 変数呼び出しは shadow 除外せず、未更新 caller (tap.ts) がある限り blocking 維持。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// shadow 除外で全参照が消える (対象 API 自体は未使用で、別ファイルの同名ローカル関数と
/// その呼び出ししか無い) 場合は closed にしない — 対象 caller の追随を 1 件も確認して
/// いないため blocking を維持する (codex レビュー指摘の fail-open 回帰テスト)。
#[test]
fn detect_api_changes_ts_all_refs_shadowed_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
            ),
        ],
        "base",
    );
    // 対象 API のみシグネチャ変更。対象 API への呼び出しはリポジトリに存在しない。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/capture.ts".to_string(),
        new_path: "src/lib/capture.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "shadow 除外で参照 0 件なら closed に降格しない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// `obj.startRecording()` (property 位置) の diff 外参照は shadow 除外できず blocking 維持
/// (member 経由は対象 API への参照か静的に判定できないため fail-closed)。
#[test]
fn detect_api_changes_ts_member_call_outside_diff_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nexport const api = { run: startRecording };\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
            (
                "src/other.ts",
                "const recorder = { startRecording: (n: number) => n };\nexport function misc() {\n    return recorder.startRecording(1);\n}\n",
            ),
        ],
        "base",
    );
    // capture.ts + content.ts (対象 caller) を変更。other.ts の `recorder.startRecording(1)`
    // (property 位置、diff 外) が shadow 除外されず blocking に倒すことを確認する。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30, cursor: true });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 1,
                new_start: 4,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "property 位置の diff 外参照は除外できないため blocking 維持。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn detect_api_changes_ts_equivalent_literal_union_alias_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export type Base = \"x\" | \"y\";\n",
        "export type Category = \"x\" | \"y\";\n"
    );
    fs::write(src_dir.join("types.ts"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/types.ts"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    let after = concat!(
        "export type Base = \"y\" | \"x\" | \"x\";\n",
        "export type Category = (Base);\n"
    );
    fs::write(src_dir.join("types.ts"), after).expect("write after");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/types.ts".to_string(),
        new_path: "src/types.ts".to_string(),
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
        api_changes
            .modified
            .iter()
            .all(|change| change.name != "Category"),
        "値集合が同値な alias 化は blocking に残してはならない: {:?}",
        api_changes.modified
    );
    let compatible = api_changes
        .compatible_modified
        .iter()
        .find(|change| change.name == "Category")
        .expect("Category must be classified as compatible");
    assert_eq!(compatible.reason, "equivalent_literal_union_alias");
}

#[test]
fn detect_api_changes_ts_literal_union_widening_and_import_alias_stay_modified() {
    for (case, before, after) in [
        (
            "widening",
            "export type Category = \"x\" | \"y\";\n",
            "export type Category = \"x\" | \"y\" | \"z\";\n",
        ),
        (
            "import alias",
            "export type Category = \"x\" | \"y\";\n",
            concat!(
                "import type { Base } from \"./shared\";\n",
                "export type Category = Base;\n"
            ),
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let src_dir = repo.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        fs::write(src_dir.join("types.ts"), before).expect("write before");
        fs::write(
            src_dir.join("shared.ts"),
            "export type Base = \"x\" | \"y\";\n",
        )
        .expect("write shared");
        assert!(
            Command::new("git")
                .args(["add", "src/types.ts", "src/shared.ts"])
                .current_dir(repo)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "initial"])
                .current_dir(repo)
                .status()
                .expect("git commit")
                .success()
        );
        fs::write(src_dir.join("types.ts"), after).expect("write after");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/types.ts".to_string(),
            new_path: "src/types.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: after.lines().count(),
            }],
            deleted_old_source: None,
        }];

        let api_changes =
            detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api_changes
                .modified
                .iter()
                .any(|change| change.name == "Category"),
            "{case} は証明範囲外なので blocking を維持する: {api_changes:?}"
        );
        assert!(
            api_changes
                .compatible_modified
                .iter()
                .all(|change| change.name != "Category"),
            "{case} を互換変更へ誤降格してはならない: {api_changes:?}"
        );
    }
}
