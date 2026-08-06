//! React HOC ラップ・object member 削除を降格する互換判定のテスト。

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

/// React.memo (named function expression) の関数本体内の lexical const は api.add に出さない。
/// (レポート 2026-05-04-next-page-and-react-memo-false-positives.md パターン1 の再現)
/// `export const X = memo(function X() { const inner = ... })` の `inner` は
/// 関数本体スコープのローカル変数で公開 API ではない。`is_js_function_body` の
/// `function_expression` 認識で境界停止される。
#[test]
fn detect_api_changes_excludes_memo_wrapper_internal_const() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    std::fs::write(
        repo.join("Card.tsx"),
        "import { memo } from 'react';\n\
export const TaskKanbanCard = memo(function TaskKanbanCard() {\n\
  const hasAssignee = true;\n\
  const milestoneColor = hasAssignee ? 'red' : 'gray';\n\
  return null;\n\
});\n",
    )
    .expect("write");

    let syms = extract_new_file_facts(repo.to_str().expect("utf-8 path"), "Card.tsx")
        .exported
        .expect("symbols");
    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(
        !names.contains(&"hasAssignee"),
        "memo wrapper 内のローカル const は exported API に含めない。got: {names:?}"
    );
    assert!(
        !names.contains(&"milestoneColor"),
        "memo wrapper 内のローカル const は exported API に含めない。got: {names:?}"
    );
    assert!(
        names.contains(&"TaskKanbanCard"),
        "memo で包んだ exported const 自体は API に含める。got: {names:?}"
    );
}

/// React.memo ラップで宣言種別が function_declaration → lexical_declaration に変わった
/// api.mod は、props 型 (複数行 destructured 含む)・JSX 利用互換なら compatible
/// (react_component_wrapper) に降格する。(レポート 2026-06-02-react-memo-api-mod.md の再現)
#[test]
fn detect_react_wrapper_multiline_destructured_props_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    // old: export function (複数行 destructured props) + JSX のみで参照するファイル
    git_commit_files(
        repo,
        &[
            (
                "ScheduleItem.tsx",
                "export function ScheduleItem({\n  a,\n  b,\n}: ScheduleItemProps) {\n  return null;\n}\n",
            ),
            (
                "TrayPopup.tsx",
                "import { ScheduleItem } from './ScheduleItem';\nexport function TrayPopup() {\n  return <ScheduleItem a={1} b={2} />;\n}\n",
            ),
        ],
        "initial",
    );
    // new: memo ラップ (working tree)
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem({\n  a,\n  b,\n}: ScheduleItemProps) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "ScheduleItem.tsx",
            new_path: "ScheduleItem.tsx",
            name: "ScheduleItem",
            kind: "constant",
            old_sig: "export function ScheduleItem({}: ScheduleItemProps)",
            new_sig: "export const ScheduleItem = memo(function ScheduleItem({",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    let compat = result.expect("複数行 destructured props でも memo ラップのみなら compatible");
    assert_eq!(compat.reason, "react_component_wrapper");
    assert_eq!(compat.name, "ScheduleItem");
}

/// memo ラップでもシンボルが関数として直接呼び出されている (`X(...)`) 場合は
/// MemoExoticComponent 化で壊れ得るため blocking (api.mod) を維持する。
#[test]
fn detect_react_wrapper_with_call_usage_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "ScheduleItem.tsx",
                "export function ScheduleItem(props: P) {\n  return null;\n}\n",
            ),
            (
                "usage.tsx",
                "import { ScheduleItem } from './ScheduleItem';\nconst rendered = ScheduleItem({});\n",
            ),
        ],
        "initial",
    );
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem(props: P) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "ScheduleItem.tsx",
            new_path: "ScheduleItem.tsx",
            name: "ScheduleItem",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(
        result.is_none(),
        "X(...) 直接呼び出しがあれば blocking 維持 (MemoExoticComponent 非互換)"
    );
}

/// props 型が変わった場合は互換でないため blocking を維持する。
#[test]
fn detect_react_wrapper_changed_props_type_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "ScheduleItem.tsx",
                "export function ScheduleItem(props: OldProps) {\n  return null;\n}\n",
            ),
            (
                "TrayPopup.tsx",
                "import { ScheduleItem } from './ScheduleItem';\nexport const x = <ScheduleItem />;\n",
            ),
        ],
        "initial",
    );
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem(props: NewProps) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "ScheduleItem.tsx",
            new_path: "ScheduleItem.tsx",
            name: "ScheduleItem",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(result.is_none(), "props 型が変われば blocking 維持");
}

#[test]
fn extract_component_props_type_handles_function_and_memo_wrapper() {
    // 複数行 destructured props + function 宣言
    let func = b"export function X({\n  a,\n  b,\n}: MyProps) { return null; }\n";
    assert_eq!(
        extract_component_props_type(func, crate::language::LangId::Tsx, "X").as_deref(),
        Some(": MyProps")
    );
    // memo ラップ (内側 function の第1引数を見る)
    let memo = b"import { memo } from 'react';\nexport const X = memo(function X({\n  a,\n}: MyProps) { return null; });\n";
    assert_eq!(
        extract_component_props_type(memo, crate::language::LangId::Tsx, "X").as_deref(),
        Some(": MyProps")
    );
    // 型注釈なしは None (blocking 維持)
    let no_type = b"export function X(props) { return null; }\n";
    assert_eq!(
        extract_component_props_type(no_type, crate::language::LangId::Tsx, "X"),
        None
    );
}

/// 定義ファイル内に named function expression 以外の値利用 (`X({})` 呼び出し) が残る
/// 場合は MemoExoticComponent 化で壊れ得るため blocking 維持 (codex 指摘: def_file 全体
/// 除外でなく named fn 名だけ safe)。
#[test]
fn detect_react_wrapper_same_file_value_usage_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "ScheduleItem.tsx",
            "export function ScheduleItem(props: P) {\n  return null;\n}\nconst probe = ScheduleItem({});\n",
        )],
        "initial",
    );
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem(props: P) {\n  return null;\n});\nconst probe = ScheduleItem({});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "ScheduleItem.tsx",
            new_path: "ScheduleItem.tsx",
            name: "ScheduleItem",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(
        result.is_none(),
        "同一ファイル内の値呼び出し ScheduleItem({{}}) があれば blocking 維持"
    );
}

/// old 側が既に wrapper (forwardRef) の wrapper-to-wrapper 変更は、ref 型等の差分を
/// 取りこぼすため blocking 維持 (codex 指摘)。
#[test]
fn detect_react_wrapper_old_already_wrapper_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Btn.tsx",
                "import { forwardRef } from 'react';\nexport const Btn = forwardRef(function Btn(props: P, ref: RefA) {\n  return null;\n});\n",
            ),
            (
                "App.tsx",
                "import { Btn } from './Btn';\nexport const x = <Btn />;\n",
            ),
        ],
        "initial",
    );
    fs::write(
            repo.join("Btn.tsx"),
            "import { forwardRef } from 'react';\nexport const Btn = forwardRef(function Btn(props: P, ref: RefB) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["Btn".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "Btn.tsx",
            new_path: "Btn.tsx",
            name: "Btn",
            kind: "constant",
            old_sig: "export const Btn = forwardRef(function Btn(props: P, ref: RefA) {",
            new_sig: "export const Btn = forwardRef(function Btn(props: P, ref: RefB) {",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(
        result.is_none(),
        "old が既に wrapper (wrapper-to-wrapper) なら blocking 維持"
    );
}

/// exported object のプロパティ削除で、削除キーへの member access が repo 全体で 0 件なら
/// compatible (unused_object_members) に降格する。(レポート 2026-06-02-provider-avatar の再現)
#[test]
fn detect_object_members_removed_unreferenced_key_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    // old: record value に bgColor。参照側は label のみ使用 (bgColor は未参照)
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google.label;\n",
            ),
        ],
        "initial",
    );
    // new: bgColor 削除 + bgClass 追加
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 'G', bgClass: 'bg-green' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "providerConfig",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    let compat = result.expect("削除キー bgColor が未参照なら compatible");
    assert_eq!(compat.reason, "unused_object_members");
}

/// 削除されたキーが member access (`config.google.bgColor`) で残存していれば破壊的なので
/// blocking 維持。
#[test]
fn detect_object_members_removed_referenced_key_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google.bgColor;\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 'G', bgClass: 'bg-green' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "providerConfig",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(
        result.is_none(),
        "削除キー bgColor が member access で残存 → blocking 維持"
    );
}

/// flat object で「既存キーの値の差し替え」と「無関係なキー追加」が同一 diff に同居する
/// ケース。キー集合の差分だけを見ていると追加のみの変更として降格され、呼び出し側の
/// `handlers.onSave()` が実行時に壊れる (false negative)。
#[test]
fn detect_object_members_value_change_with_added_key_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "handlers.ts",
                "export const handlers = { onSave: () => save() };\nfunction save() {}\n",
            ),
            (
                "user.ts",
                "import { handlers } from './handlers';\nhandlers.onSave();\n",
            ),
        ],
        "initial",
    );
    // onSave が function → number に差し替わり、同時に無関係な onLoad が増える
    fs::write(
        repo.join("handlers.ts"),
        "export const handlers = { onSave: 42, onLoad: () => load() };\nfunction save() {}\nfunction load() {}\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "handlers.ts",
            new_path: "handlers.ts",
            name: "handlers",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Typescript),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(
        result.is_none(),
        "残存キー onSave の値が変わっている → キー追加が同居していても blocking 維持"
    );
}

/// record でも、両側に残存する (record key, member key) の値が変わっていれば blocking。
/// `google.label` は参照されており、callable → 数値の差し替えは破壊的。
#[test]
fn detect_object_members_record_retained_member_value_change_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: () => 'G', bgColor: 'green' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google.label();\n",
            ),
        ],
        "initial",
    );
    // label が callable → 数値。同時に record entry を 1 件追加する
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 42, bgColor: 'green' },\n  github: { label: () => 'H', bgColor: 'black' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "providerConfig",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(
        result.is_none(),
        "残存 record entry google.label の値が変わっている → blocking 維持"
    );
}

/// record entry の純粋追加 (既存 entry の値は不変) は従来どおり降格する。
/// 上の値変更テストと対で、値照合が過剰に blocking へ倒れていないことを固定する。
#[test]
fn detect_object_members_record_pure_entry_addition_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: 'G' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google.label;\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 'G' },\n  github: { label: 'H' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "providerConfig",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    let compat = result.expect("既存 entry が不変なら record entry 追加は compatible");
    assert_eq!(compat.reason, "unused_object_members");
}

/// key 集合が完全一致する initializer 値のみ変更は unused_object_members ではないため
/// blocking 維持。
#[test]
fn detect_object_members_value_only_change_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("config.tsx", "export const c = { enabled: true };\n")],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const c = { enabled: false };\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "c",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(result.is_none(), "値のみ変更は blocking 維持");
}

/// 削除 key なし、追加 key ありの純粋な member 追加は compatible に降格する。
#[test]
fn detect_object_members_pure_member_addition_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("config.tsx", "export const c = { enabled: true };\n")],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const c = { enabled: true, mode: 'dark' };\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "c",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    let compat = result.expect("純粋な member 追加は compatible");
    assert_eq!(compat.reason, "unused_object_members");
}

/// record value の schema が不揃いなら、同名 key が別 record entry に残っていても
/// 削除有無を安全に判断できないため blocking 維持。
#[test]
fn detect_object_members_record_schema_mismatch_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "config.tsx",
            "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n  openai: { label: 'O', bgColor: 'blue' },\n};\n",
        )],
        "initial",
    );
    fs::write(
            repo.join("config.tsx"),
            "export const providerConfig = {\n  google: { label: 'G' },\n  openai: { label: 'O', bgColor: 'blue' },\n};\n",
        )
        .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "providerConfig",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(result.is_none(), "record schema 不揃いは blocking 維持");
}

/// 削除された key が文字列 bracket access (`obj["key"]`) で残存していれば破壊的なので
/// blocking 維持。
#[test]
fn detect_object_members_removed_bracket_string_ref_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google[\"bgColor\"];\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 'G' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "providerConfig",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(
        result.is_none(),
        "削除キー bgColor が bracket string access で残存 → blocking 維持"
    );
}

/// spread (`...base`) を含む object は shape を静的確定できないため blocking 維持。
#[test]
fn detect_object_members_spread_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("config.tsx", "export const c = { ...base, a: 1 };\n")],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const c = { ...base, b: 2 };\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        &CompatibleModSite {
            dir: repo.to_str().expect("utf-8 path"),
            base: "HEAD",
            old_path: "config.tsx",
            new_path: "config.tsx",
            name: "c",
            kind: "constant",
            old_sig: "old",
            new_sig: "new",
            lang_id: Some(crate::language::LangId::Tsx),
        },
        &mut SignatureSourceCache::default(),
    );
    assert!(result.is_none(), "spread を含む object は blocking 維持");
}

#[test]
fn extract_object_member_keys_collects_record_value_keys() {
    // record 形式: top-level は record entry、value object のキー (label/bgColor) を schema とする
    let src = b"export const c = {\n  google: { label: 'G', bgColor: 'green' },\n  openai: { label: 'O', bgColor: 'blue' },\n};\n";
    let keys =
        extract_object_member_keys(src, crate::language::LangId::Tsx, "c").expect("record keys");
    assert!(keys.member_keys.contains("label"));
    assert!(keys.member_keys.contains("bgColor"));
    let record_keys = keys.record_keys.expect("record keys");
    assert!(record_keys.contains("google"));
    assert!(record_keys.contains("openai"));
    // spread を含むと None (blocking)
    let spread = b"export const c = { ...base, a: 1 };\n";
    assert!(extract_object_member_keys(spread, crate::language::LangId::Tsx, "c").is_none());
}

#[test]
fn new_sig_has_react_wrapper_detects_hocs() {
    assert!(new_sig_has_react_wrapper(
        "export const X = memo(function X() {"
    ));
    assert!(new_sig_has_react_wrapper(
        "export const X = React.forwardRef(function X() {"
    ));
    // 単なる function 宣言や部分一致 (somememo) はラッパーでない
    assert!(!new_sig_has_react_wrapper("export function X(props: T)"));
    assert!(!new_sig_has_react_wrapper("export const X = somememo(fn)"));
}

#[test]
fn ctx_usage_classification_jsx_vs_value() {
    // JSX タグ利用は safe
    assert!(ctx_usage_is_jsx_or_safe(
        "return <ScheduleItem foo={1} />;",
        "ScheduleItem"
    ));
    assert!(ctx_usage_is_jsx_or_safe(
        "  </ScheduleItem>",
        "ScheduleItem"
    ));
    // 値利用は blocking 側
    assert!(!ctx_usage_is_jsx_or_safe(
        "const x = ScheduleItem({});",
        "ScheduleItem"
    ));
    assert!(!ctx_usage_is_jsx_or_safe(
        "typeof ScheduleItem",
        "ScheduleItem"
    ));
    assert!(!ctx_usage_is_jsx_or_safe(
        "ScheduleItem.displayName = 'x';",
        "ScheduleItem"
    ));
    // 裸の代入は判定不能なので blocking 側
    assert!(!ctx_usage_is_jsx_or_safe(
        "const Alias = ScheduleItem;",
        "ScheduleItem"
    ));
}
