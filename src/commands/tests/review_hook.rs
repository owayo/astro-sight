//! `build_review_hook_json` の blocking / informational 分類のテスト。

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

/// compatible_modified (mod_compat) のみの api 変更は informational として hook JSON に
/// 出すが blocking にはしない。
#[test]
fn build_review_hook_json_compatible_modified_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: vec![CompatibleApiModification {
                name: "ScheduleItem".to_string(),
                kind: "constant".to_string(),
                file: "ScheduleItem.tsx".to_string(),
                old_signature: None,
                new_signature: None,
                reason: "react_component_wrapper".to_string(),
            }],
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };
    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "mod_compat は情報提供として hook JSON に出すべき"
    );
    assert!(!build.is_blocking, "mod_compat (互換変更) は非 blocking");
}

/// mod_compat に分類済みのシンボルに紐づく cross-file impact は、破壊的影響ではなく
/// 参考情報として扱う。api 側だけ非 blocking でも impacts が残ると Stop hook が
/// 自己矛盾した blocking 出力になるため、同じシンボルの impacts から除外する。
#[test]
fn build_review_hook_json_compatible_modified_impact_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("TaskDetailHeader.tsx"),
        "export const TaskDetailHeader = memo(function TaskDetailHeader() { return null; });\n",
    )
    .expect("write changed file");
    fs::write(
        dir.path().join("TaskDetailContent.tsx"),
        "export const view = <TaskDetailHeader />;\n",
    )
    .expect("write caller file");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "TaskDetailHeader.tsx".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "TaskDetailHeader".to_string(),
                    kind: "function".to_string(),
                    change_type: "modified".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "TaskDetailContent.tsx".to_string(),
                    name: "TaskDetailContent".to_string(),
                    line: 1,
                    symbols: vec!["TaskDetailHeader".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: vec![CompatibleApiModification {
                name: "TaskDetailHeader".to_string(),
                kind: "function".to_string(),
                file: "TaskDetailHeader.tsx".to_string(),
                old_signature: Some("export function TaskDetailHeader()".to_string()),
                new_signature: Some(
                    "export const TaskDetailHeader = memo(function TaskDetailHeader()".to_string(),
                ),
                reason: "react_component_wrapper".to_string(),
            }],
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(
        !build.is_blocking,
        "mod_compat 起因の impact だけなら Stop hook を止めないべき"
    );
    assert!(
        hook_json.get("impacts").is_none(),
        "mod_compat と同じシンボルの impacts は hook の blocking 出力から除外されるべき"
    );
    assert_eq!(
        hook_json["api"]["mod_compat"][0]["reason"],
        "react_component_wrapper"
    );
}

#[test]
fn build_review_hook_json_mixed_compatible_and_breaking_impact_keeps_breaking_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("TaskDetailHeader.tsx"),
        "export const TaskDetailHeader = memo(function TaskDetailHeader() { return null; });\nexport function loadTask(id: string, required: boolean) {}\n",
    )
    .expect("write changed file");
    fs::write(
        dir.path().join("TaskDetailContent.tsx"),
        "export const view = <TaskDetailHeader />;\nloadTask('1');\n",
    )
    .expect("write caller file");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "TaskDetailHeader.tsx".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![
                    crate::models::impact::AffectedSymbol {
                        name: "TaskDetailHeader".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "loadTask".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                ],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "TaskDetailContent.tsx".to_string(),
                    name: "TaskDetailContent".to_string(),
                    line: 2,
                    symbols: vec!["TaskDetailHeader".to_string(), "loadTask".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: vec![ApiSymbolChange {
                name: "loadTask".to_string(),
                kind: "function".to_string(),
                file: "TaskDetailHeader.tsx".to_string(),
                old_signature: Some("export function loadTask(id: string)".to_string()),
                new_signature: Some(
                    "export function loadTask(id: string, required: boolean)".to_string(),
                ),
                no_resolved_internal_callers: false,
            }],
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: vec![CompatibleApiModification {
                name: "TaskDetailHeader".to_string(),
                kind: "function".to_string(),
                file: "TaskDetailHeader.tsx".to_string(),
                old_signature: Some("export function TaskDetailHeader()".to_string()),
                new_signature: Some(
                    "export const TaskDetailHeader = memo(function TaskDetailHeader()".to_string(),
                ),
                reason: "react_component_wrapper".to_string(),
            }],
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(
        build.is_blocking,
        "破壊的 api.mod が同じ caller にあれば blocking を維持すべき"
    );
    let impacts = hook_json["impacts"].as_array().expect("impacts array");
    assert_eq!(impacts[0]["syms"], serde_json::json!(["loadTask"]));
    assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["loadTask"]));
}

#[test]
fn build_review_hook_json_returns_none_when_no_issues() {
    let dir = tempfile::tempdir().expect("tempdir");

    let build = build_review_hook_json(
        &ReviewResult::default(),
        dir.path().to_str().expect("utf-8 path"),
        false,
    );
    assert!(
        build.value.is_none(),
        "問題がない review 結果では hook JSON を生成しないべき"
    );
    assert!(!build.is_blocking, "出力なしなら blocking にしないべき");
}

/// cochange のみの場合は出力はするが exit 1 にはしない (informational)
#[test]
fn build_review_hook_json_cochange_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: vec![MissingCochange {
            file: "a.rs".to_string(),
            expected_with: "b.rs".to_string(),
            confidence: 0.9,
            co_changes: 9,
            denominator: Some(10),
        }],
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "cochange は情報提供として JSON 出力はするべき"
    );
    assert!(
        !build.is_blocking,
        "cochange のみの場合は Stop hook を止めないべき"
    );

    // 標本の大きさを出力から読めるようにする。`c` (confidence %) だけでは 1/1 と 9/10 の
    // 区別がつかず、「共変更 90%」の表示だけでトリアージが判断してしまう。
    let entry = build.value.expect("hook JSON")["cochange"][0].clone();
    assert_eq!(entry["c"], 90, "confidence は百分率");
    assert_eq!(entry["n"], 9, "共変更コミット数 (分子) を併記する");
    assert_eq!(entry["d"], 10, "集計対象コミット数 (分母) を併記する");
}

/// 分母が算出できなかった missing_cochange では `d` を省略する
/// (存在しない値を 0 として出すと「分母 0」と読めてしまう)。
#[test]
fn build_review_hook_json_cochange_omits_denominator_when_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: vec![MissingCochange {
            file: "a.rs".to_string(),
            expected_with: "b.rs".to_string(),
            confidence: 1.0,
            co_changes: 3,
            denominator: None,
        }],
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let entry = build.value.expect("hook JSON")["cochange"][0].clone();
    assert_eq!(entry["n"], 3, "分子は常に出す");
    assert!(
        entry.get("d").is_none(),
        "分母が不明なら省略する: {entry:?}"
    );
}

/// import-only などの informational impact は hook JSON に出すが、blocking にはしない。
#[test]
fn build_review_hook_json_impact_info_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(
        src_dir.join("lib.ts"),
        "export function compute() { return 1; }\n",
    )
    .expect("write changed file");
    fs::write(
        src_dir.join("consumer.ts"),
        "import { compute } from './lib';\n",
    )
    .expect("write caller file");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/lib.ts".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "compute".to_string(),
                    kind: "function".to_string(),
                    change_type: "modified".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: Vec::new(),
                low_confidence_callers: Vec::new(),
                informational_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/consumer.ts".to_string(),
                    name: "compute".to_string(),
                    line: 1,
                    symbols: vec!["compute".to_string()],
                    confidence: Some("informational".to_string()),
                }],
            }],
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("impact_info should be emitted");
    assert!(
        !build.is_blocking,
        "impact_info だけなら Stop hook を止めないべき"
    );
    assert!(
        hook_json.get("impacts").is_none(),
        "informational impact は blocking impacts には混ぜない"
    );
    assert_eq!(
        hook_json["impact_info"][0]["refs"][0]["s"],
        serde_json::json!(["compute"])
    );
}

/// api.add のみの場合は informational として出力されるが blocking にはしない
#[test]
fn build_review_hook_json_api_add_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: vec![ApiSymbol {
                name: "foo".to_string(),
                kind: "function".to_string(),
                file: "a.rs".to_string(),
                refs_internal: 0,
            }],
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(build.value.is_some(), "api.add は hook JSON に出すべき");
    assert!(
        !build.is_blocking,
        "api.add のみ (additive) は Stop hook を止めないべき"
    );
}

/// api.add には抽出条件の自己記述 `add_scope` が付き、`add` が空なら付かない。
/// これが無いと「載らない新規 export = 検出漏れ」と誤認したトリアージが走る
/// (Issue 2026-07-27-api-add-scope-not-visible)
#[test]
fn build_review_hook_json_api_add_carries_extraction_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let empty_api = || ApiChanges {
        added: Vec::new(),
        removed: Vec::new(),
        modified: Vec::new(),
        moved: Vec::new(),
        property_to_field: Vec::new(),
        removed_dead: Vec::new(),
        modified_closed_in_diff: Vec::new(),
        const_value_changes: Vec::new(),
        compatible_modified: Vec::new(),
    };
    let review_with = |api_changes: ApiChanges| ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes,
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let with_add = review_with(ApiChanges {
        added: vec![ApiSymbol {
            name: "foo".to_string(),
            kind: "function".to_string(),
            file: "a.rs".to_string(),
            refs_internal: 0,
        }],
        ..empty_api()
    });
    let build = build_review_hook_json(&with_add, dir.path().to_str().expect("utf-8 path"), false);
    let api = build.value.expect("hook JSON")["api"].clone();
    assert_eq!(
        api["add_scope"], "no_cross_file_refs_in_diff",
        "api.add の抽出条件 (同一 diff の他ファイルから実利用参照なし) を出力から読めるようにする"
    );

    // `add` が空なら `add_scope` も出さない (無意味なトークンを増やさない)
    let without_add = review_with(ApiChanges {
        removed: vec![ApiSymbol {
            name: "bar".to_string(),
            kind: "function".to_string(),
            file: "a.rs".to_string(),
            refs_internal: 0,
        }],
        ..empty_api()
    });
    let build = build_review_hook_json(
        &without_add,
        dir.path().to_str().expect("utf-8 path"),
        false,
    );
    let api = build.value.expect("hook JSON")["api"].clone();
    assert!(
        api.get("add_scope").is_none(),
        "add が空なら add_scope も省略する"
    );
}

/// api.add の各シンボルには同一ファイル内の実利用参照数 `ri` が付き、0 なら省略される。
/// `add_scope` は「同一 diff の他ファイルから参照されていない」抽出範囲しか表せず、
/// 同一ファイル内の型注釈参照があるケースを「完全に未参照」と読み違えたトリアージが
/// 走った (Issue 2026-08-04-review-add-scope-naming)
#[test]
fn build_review_hook_json_api_add_carries_internal_ref_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    let review_with_added = |added: Vec<ApiSymbol>| ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added,
            ..Default::default()
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    // 同一ファイル内に実利用参照が 2 件 → `ri: 2` を出して refs 再実行を不要にする
    let with_internal_refs = review_with_added(vec![ApiSymbol {
        name: "TypeObservation".to_string(),
        kind: "interface".to_string(),
        file: "module.ts".to_string(),
        refs_internal: 2,
    }]);
    let build = build_review_hook_json(
        &with_internal_refs,
        dir.path().to_str().expect("utf-8 path"),
        false,
    );
    let api = build.value.expect("hook JSON")["api"].clone();
    assert_eq!(
        api["add"][0]["ri"], 2,
        "同一ファイル内の実利用参照数を api.add に添えるべき: {api}"
    );

    // 完全に未参照なら `ri` は省略する (compact 規約: 0 は出さない)
    let without_internal_refs = review_with_added(vec![ApiSymbol {
        name: "Orphan".to_string(),
        kind: "interface".to_string(),
        file: "module.ts".to_string(),
        refs_internal: 0,
    }]);
    let build = build_review_hook_json(
        &without_internal_refs,
        dir.path().to_str().expect("utf-8 path"),
        false,
    );
    let api = build.value.expect("hook JSON")["api"].clone();
    assert!(
        api["add"][0].get("ri").is_none(),
        "参照 0 件なら ri を省略する (トークンを増やさない): {api}"
    );
}

/// 打ち切り (未追跡の巨大ファイル除外) は hook 出力の `trunc` に載り、blocking にはしない。
/// hook で沈黙させると「レビュー範囲が欠けたこと」に気付けず「全部見た」と読める
/// (Issue 2026-08-04-review-git-untracked-huge-file-blowup)
#[test]
fn build_review_hook_json_reports_truncations_without_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        truncations: vec![
            crate::models::truncation::TruncationInfo::untracked_file_too_large(
                "generated.rs",
                "lines",
                80_000,
                5_000,
            ),
        ],
        ..Default::default()
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let value = build.value.expect("打ち切りだけでも hook 出力を出すべき");
    assert_eq!(
        value["trunc"][0]["f"], "generated.rs",
        "除外したファイルを hook にも出すべき: {value}"
    );
    assert_eq!(
        value["trunc"][0]["r"], "untracked_file_too_large",
        "打ち切り理由を機械可読キーで出すべき: {value}"
    );
    assert!(
        !build.is_blocking,
        "打ち切りは検出ではなく解析範囲の申告なので Stop hook を止めない"
    );

    // 打ち切りが無ければ `trunc` キー自体を出さない (compact 規約)。
    let clean = ReviewResult::default();
    let build = build_review_hook_json(&clean, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_none(),
        "検出も打ち切りも無ければ hook は無出力"
    );
}

/// api.removed は破壊的変更の可能性があるため blocking になる
#[test]
fn build_review_hook_json_api_removed_is_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: vec![ApiSymbol {
                name: "foo".to_string(),
                kind: "function".to_string(),
                file: "a.rs".to_string(),
                refs_internal: 0,
            }],
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(build.value.is_some(), "api.rm は hook JSON に出すべき");
    assert!(build.is_blocking, "api.rm は blocking にすべき");
}

/// api.modified は破壊的変更の可能性があるため blocking になる
#[test]
fn build_review_hook_json_api_modified_is_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: vec![ApiSymbolChange {
                name: "foo".to_string(),
                kind: "function".to_string(),
                file: "a.rs".to_string(),
                old_signature: Some("fn foo()".to_string()),
                new_signature: Some("fn foo(x: u32)".to_string()),
                no_resolved_internal_callers: false,
            }],
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("api.mod は hook JSON に出すべき");
    assert!(build.is_blocking, "api.mod は blocking にすべき");
    assert!(
        hook_json["api"]["mod"][0].get("no_callers").is_none(),
        "呼び出し参照ありなら no_callers は省略される: {hook_json}"
    );
}

/// 解決できた呼び出し参照が 0 件の api.mod には `no_callers` を添える。
/// 分類 (`api.mod`) も blocking も変えず、トリアージ用の情報だけ足す
/// (Issue 2026-08-05-api-mod-callers-updated-indirectly のパターン B)。
#[test]
fn build_review_hook_json_api_modified_without_callers_is_flagged_but_still_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: vec![ApiSymbolChange {
                name: "foo".to_string(),
                kind: "function".to_string(),
                file: "a.rs".to_string(),
                old_signature: Some("fn foo()".to_string()),
                new_signature: Some("fn foo(x: u32)".to_string()),
                no_resolved_internal_callers: true,
            }],
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("api.mod は hook JSON に出すべき");
    assert_eq!(
        hook_json["api"]["mod"][0]["no_callers"],
        serde_json::json!(true),
        "呼び出し参照 0 件は no_callers で示す: {hook_json}"
    );
    assert!(
        build.is_blocking,
        "呼び出し参照 0 件でも api.mod は blocking のまま (外部利用・動的呼び出しと区別できない)"
    );
}

/// `rm_dead` (削除前参照ゼロの dead symbol 削除) は破壊的変更ではないため、
/// 単独では Stop hook を blocking しない (informational)。moon-star-link 報告
/// (2026-06-13) の「rm_dead が hook failure の原因」は誤診断で、実際の blocking は
/// 同時に出ていた api.mod (Kotlin body-only 誤検出) が原因だった。rm_dead が
/// 非 blocking である契約を回帰テストで固定する。
#[test]
fn build_review_hook_json_removed_dead_only_is_not_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: vec![
                ApiSymbol {
                    name: "MapZoomUtils".to_string(),
                    kind: "object".to_string(),
                    file: "MapZoomUtils.kt".to_string(),
                    refs_internal: 0,
                },
                ApiSymbol {
                    name: "MapZoomUtils.zoomForBounds".to_string(),
                    kind: "function".to_string(),
                    file: "MapZoomUtils.kt".to_string(),
                    refs_internal: 0,
                },
            ],
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "rm_dead は informational として hook JSON に出すべき"
    );
    assert!(
        !build.is_blocking,
        "rm_dead 単独は blocking にすべきでない (informational)"
    );
}

#[test]
fn build_review_hook_json_const_value_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: vec![ApiSymbolChange {
                name: "ENEMY_SPEED".to_string(),
                kind: "constant".to_string(),
                file: "src/constants.rs".to_string(),
                old_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                new_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                no_resolved_internal_callers: false,
            }],
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };
    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "const_value 変更は informational として hook JSON に出すべき"
    );
    assert!(
        !build.is_blocking,
        "const_value のみの変更はデフォルトで blocking にしないべき"
    );
}

#[test]
fn build_review_hook_json_const_value_is_blocking_under_strict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: vec![ApiSymbolChange {
                name: "ENEMY_SPEED".to_string(),
                kind: "constant".to_string(),
                file: "src/constants.rs".to_string(),
                old_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                new_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                no_resolved_internal_callers: false,
            }],
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };
    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), true);
    assert!(
        build.is_blocking,
        "--strict-public-const-values 指定時は const_value を blocking に昇格すべき"
    );
}

#[test]
fn build_review_hook_json_uses_changed_symbols_in_summary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("lib.rs"), "pub fn compute() {}\n").expect("write changed file");
    fs::write(src_dir.join("main.rs"), "fn main() { compute(); }\n").expect("write caller");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/lib.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "compute".to_string(),
                    kind: "function".to_string(),
                    change_type: "modified".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/main.rs".to_string(),
                    name: "main".to_string(),
                    line: 1,
                    // caller.symbols は「この caller が参照している、変更ファイル内の
                    // シンボル名」(pass3.rs::build_file_impact の構築意図)。
                    // 呼び出し元関数の名前は ImpactedCaller.name 側に入る。
                    symbols: vec!["compute".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(build.is_blocking, "impacts があれば blocking にすべき");
    let impacts = hook_json["impacts"]
        .as_array()
        .expect("impacts should be an array");
    assert_eq!(impacts.len(), 1);
    assert_eq!(impacts[0]["src"], "src/lib.rs");
    assert_eq!(impacts[0]["syms"], serde_json::json!(["compute"]));
    assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["compute"]));
}

/// hook の `syms` には cross-file caller を発生させた causal symbol だけを残し、
/// 非 export const や本体未変更の export を除外する (Issue 2026-05-14
/// private-const-and-unchanged-export-noise)。
#[test]
fn build_review_hook_json_filters_non_causal_affected_symbols_from_syms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("a.rs"), "pub fn foo() {}\n").expect("write changed file");
    fs::write(src_dir.join("b.rs"), "fn caller() { foo(); }\n").expect("write caller");

    // affected_symbols は変更ファイル内で hunk と overlap した全シンボル。
    // PRIVATE_CONST と unchanged_export は cross-file 検索で is_symbol_exported に
    // 弾かれて caller.symbols には含まれないため、hook の syms にも出てはならない。
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/a.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![
                    crate::models::impact::AffectedSymbol {
                        name: "foo".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "PRIVATE_CONST".to_string(),
                        kind: "constant".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "unchanged_export".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                ],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/b.rs".to_string(),
                    name: "caller".to_string(),
                    line: 1,
                    symbols: vec!["foo".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(build.is_blocking, "未解決 impact があれば blocking");
    let impacts = hook_json["impacts"]
        .as_array()
        .expect("impacts should be an array");
    assert_eq!(impacts.len(), 1);
    assert_eq!(
        impacts[0]["syms"],
        serde_json::json!(["foo"]),
        "syms は cross-file caller を発生させた causal symbol だけになるべき (PRIVATE_CONST と unchanged_export は除外)"
    );
    // refs[].s は元々 caller.symbols そのまま (causal の絞り込みは不要)
    assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["foo"]));
}

/// 新規追加 (`change_type=added`) シンボルへの caller のみがある場合、
/// hook blocking には含めない。同コミットで新規シンボルと新規参照が
/// セットで導入されるのは自然な依存関係で、breaking change ではない
/// (Issue 2026-05-27-added-symbol-initial-reference)。
#[test]
fn build_review_hook_json_added_only_caller_is_not_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("constants.rs"), "pub const FOO: u32 = 1;\n").unwrap();
    fs::write(
        src_dir.join("user.rs"),
        "use crate::constants::FOO; fn x() { let _ = FOO; }\n",
    )
    .unwrap();

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/constants.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "FOO".to_string(),
                    kind: "constant".to_string(),
                    change_type: "added".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/user.rs".to_string(),
                    name: "x".to_string(),
                    line: 1,
                    symbols: vec!["FOO".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().unwrap(), false);
    // 新規追加シンボルへの caller のみ → impacts は空 (blocking 対象外)
    assert!(
        build.value.is_none() || {
            let v = build.value.as_ref().unwrap();
            v.get("impacts")
                .and_then(|i| i.as_array())
                .is_none_or(|a| a.is_empty())
        },
        "added シンボルのみへの caller は hook impacts から除外されるべき: {:?}",
        build.value
    );
    assert!(
        !build.is_blocking,
        "added のみの場合は Stop hook を止めないべき"
    );
}

/// 同 caller が added と modified の両方を参照している場合、modified だけを
/// causal symbol として残し blocking する。
#[test]
fn build_review_hook_json_mixed_added_and_modified_keeps_only_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(
        src_dir.join("a.rs"),
        "pub fn modified_fn() {}\npub const NEW_CONST: u32 = 1;\n",
    )
    .unwrap();
    fs::write(
            src_dir.join("b.rs"),
            "use crate::a::{modified_fn, NEW_CONST}; fn caller() { modified_fn(); let _ = NEW_CONST; }\n",
        )
        .unwrap();

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/a.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![
                    crate::models::impact::AffectedSymbol {
                        name: "modified_fn".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "NEW_CONST".to_string(),
                        kind: "constant".to_string(),
                        change_type: "added".to_string(),
                    },
                ],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/b.rs".to_string(),
                    name: "caller".to_string(),
                    line: 1,
                    symbols: vec!["modified_fn".to_string(), "NEW_CONST".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
            truncations: Vec::new(),
        },
        missing_cochanges: Vec::new(),
        cochange_diagnostics: Default::default(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
        truncations: Vec::new(),
    };

    let build = build_review_hook_json(&result, dir.path().to_str().unwrap(), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(build.is_blocking, "modified を含むため blocking");
    let impacts = hook_json["impacts"].as_array().expect("impacts array");
    assert_eq!(impacts.len(), 1);
    // syms / refs[].s には modified_fn のみが残り、NEW_CONST (added) は落ちる
    assert_eq!(
        impacts[0]["syms"],
        serde_json::json!(["modified_fn"]),
        "added 由来の NEW_CONST は syms から除外され modified_fn のみ残るべき"
    );
    assert_eq!(
        impacts[0]["refs"][0]["s"],
        serde_json::json!(["modified_fn"]),
        "refs[].s も modified_fn のみに絞られるべき"
    );
}
