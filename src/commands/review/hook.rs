use anyhow::Result;
use serde::Serialize;

use crate::commands::ChangedFileSet;
use crate::models::impact::ImpactedCaller;
use crate::models::review::{ApiChanges, ReviewResult};

/// `--hook` の出力判定結果。
/// - `value`: stderr に書き出す JSON (何もなければ None)
/// - `is_blocking`: exit 1 にして Stop hook を止めるべきか。cochange だけは informational
///   として block しない (レポート 2026-04-11-cochange-new-repo-initial-commit-noise.md の提案)
pub(crate) struct HookJsonBuild {
    pub value: Option<serde_json::Value>,
    pub is_blocking: bool,
}

#[derive(Default)]
struct HookImpactGroup {
    changed_symbols: std::collections::BTreeSet<String>,
    refs: Vec<(String, usize, Vec<String>)>,
}

#[derive(Serialize)]
struct HookImpactRef<'a> {
    p: &'a str,
    ln: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<&'a [String]>,
}

#[derive(Serialize)]
struct HookImpact<'a> {
    src: &'a str,
    syms: Vec<&'a String>,
    refs: Vec<HookImpactRef<'a>>,
}

#[derive(Serialize)]
struct HookNameFile<'a> {
    n: &'a str,
    f: &'a str,
}

#[derive(Serialize)]
struct HookCompatibleModification<'a> {
    n: &'a str,
    f: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct HookMovedSymbol<'a> {
    n: &'a str,
    from: &'a str,
    to: &'a str,
}

#[derive(Serialize)]
struct HookCochange<'a> {
    f: &'a str,
    w: &'a str,
    c: u32,
}

#[derive(Serialize)]
struct HookApi<'a> {
    #[serde(rename = "add", skip_serializing_if = "Vec::is_empty")]
    added: Vec<HookNameFile<'a>>,
    #[serde(rename = "rm", skip_serializing_if = "Vec::is_empty")]
    removed: Vec<HookNameFile<'a>>,
    #[serde(rename = "mod", skip_serializing_if = "Vec::is_empty")]
    modified: Vec<HookNameFile<'a>>,
    #[serde(rename = "mod_closed", skip_serializing_if = "Vec::is_empty")]
    modified_closed_in_diff: Vec<HookNameFile<'a>>,
    #[serde(rename = "const_value", skip_serializing_if = "Vec::is_empty")]
    const_value_changes: Vec<HookNameFile<'a>>,
    #[serde(rename = "mod_compat", skip_serializing_if = "Vec::is_empty")]
    compatible_modified: Vec<HookCompatibleModification<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    moved: Vec<HookMovedSymbol<'a>>,
    #[serde(rename = "rm_dead", skip_serializing_if = "Vec::is_empty")]
    removed_dead: Vec<HookNameFile<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    property_to_field: Vec<HookNameFile<'a>>,
}

impl<'a> HookApi<'a> {
    fn from_api_changes(api: &'a ApiChanges) -> Self {
        let name_file = |name: &'a String, file: &'a String| HookNameFile {
            n: name.as_str(),
            f: file.as_str(),
        };
        Self {
            added: api
                .added
                .iter()
                .map(|change| name_file(&change.name, &change.file))
                .collect(),
            removed: api
                .removed
                .iter()
                .map(|change| name_file(&change.name, &change.file))
                .collect(),
            modified: api
                .modified
                .iter()
                .map(|change| name_file(&change.name, &change.file))
                .collect(),
            modified_closed_in_diff: api
                .modified_closed_in_diff
                .iter()
                .map(|change| name_file(&change.name, &change.file))
                .collect(),
            const_value_changes: api
                .const_value_changes
                .iter()
                .map(|change| name_file(&change.name, &change.file))
                .collect(),
            compatible_modified: api
                .compatible_modified
                .iter()
                .map(|change| HookCompatibleModification {
                    n: change.name.as_str(),
                    f: change.file.as_str(),
                    reason: change.reason.as_str(),
                })
                .collect(),
            moved: api
                .moved
                .iter()
                .map(|change| HookMovedSymbol {
                    n: change.name.as_str(),
                    from: change.from.as_str(),
                    to: change.to.as_str(),
                })
                .collect(),
            removed_dead: api
                .removed_dead
                .iter()
                .map(|change| name_file(&change.name, &change.file))
                .collect(),
            property_to_field: api
                .property_to_field
                .iter()
                .map(|change| name_file(&change.name, &change.file))
                .collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.modified.is_empty()
            && self.modified_closed_in_diff.is_empty()
            && self.const_value_changes.is_empty()
            && self.compatible_modified.is_empty()
            && self.moved.is_empty()
            && self.removed_dead.is_empty()
            && self.property_to_field.is_empty()
    }
}

fn impact_groups_value(
    groups: &std::collections::BTreeMap<String, HookImpactGroup>,
) -> serde_json::Value {
    let impacts: Vec<HookImpact<'_>> = groups
        .iter()
        .map(|(changed_path, group)| HookImpact {
            src: changed_path,
            syms: group.changed_symbols.iter().collect(),
            refs: group
                .refs
                .iter()
                .map(|(path, line, symbols)| HookImpactRef {
                    p: path,
                    ln: *line,
                    s: (!symbols.is_empty()).then_some(symbols.as_slice()),
                })
                .collect(),
        })
        .collect();
    serde_json::to_value(impacts).expect("hook impact DTO should serialize")
}

pub(crate) fn build_review_hook_json(
    result: &ReviewResult,
    dir: &str,
    strict_const_values: bool,
) -> HookJsonBuild {
    // api 側で「互換 / 追随済み / 値のみ変更」と判定済みの modified 系シンボルは、
    // Stop hook の impact でも informational として扱う。ここを揃えないと api.mod_compat
    // 自体は非 blocking なのに、同じシンボルの参照一覧が impacts として blocking になる。
    let mut informational_modified_api_symbols: std::collections::HashSet<(&str, &str)> =
        std::collections::HashSet::new();
    informational_modified_api_symbols.extend(
        result
            .api_changes
            .compatible_modified
            .iter()
            .map(|m| (m.file.as_str(), m.name.as_str())),
    );
    informational_modified_api_symbols.extend(
        result
            .api_changes
            .modified_closed_in_diff
            .iter()
            .map(|m| (m.file.as_str(), m.name.as_str())),
    );
    if !strict_const_values {
        informational_modified_api_symbols.extend(
            result
                .api_changes
                .const_value_changes
                .iter()
                .map(|m| (m.file.as_str(), m.name.as_str())),
        );
    }

    // 未解決 impact を収集
    let changed_paths: std::collections::HashSet<&str> = result
        .impact
        .changes
        .iter()
        .map(|c| c.path.as_str())
        .collect();
    let changed_canonical: std::collections::HashSet<std::path::PathBuf> = changed_paths
        .iter()
        .filter_map(|cp| {
            let abs = if std::path::Path::new(cp).is_relative() {
                std::path::Path::new(dir).join(cp)
            } else {
                std::path::PathBuf::from(cp)
            };
            std::fs::canonicalize(&abs).ok()
        })
        .collect();
    let changed_abs_strs: std::collections::HashSet<String> = changed_paths
        .iter()
        .map(|cp| {
            if std::path::Path::new(cp).is_relative() {
                std::path::Path::new(dir)
                    .join(cp)
                    .to_string_lossy()
                    .to_string()
            } else {
                cp.to_string()
            }
        })
        .collect();

    let mut unresolved: std::collections::BTreeMap<String, HookImpactGroup> =
        std::collections::BTreeMap::new();
    let mut informational: std::collections::BTreeMap<String, HookImpactGroup> =
        std::collections::BTreeMap::new();
    for change in &result.impact.changes {
        if change.affected_symbols.is_empty() {
            continue;
        }
        // hook の `syms` には「実際に cross-file caller を発生させた causal シンボル」だけを残す。
        // `change.affected_symbols` を丸ごと入れると、is_symbol_exported で cross-file 検索を
        // 弾かれた非 export const や、隣接 hunk の context に巻き込まれた未変更 export まで
        // hook 出力に混ざる。`caller.symbols` (cross-file 検索を通過した causal name) と
        // `affected_symbols` の交差で causal だけを抽出する。
        //
        // また `change_type == "added"` のシンボルは「同コミットで新規追加され、まだ既存
        // 呼び出し側を持っていない export」。hook (stop blocking 判定) では「新規依存関係」
        // として価値はあるが、breaking change ではないため除外する。通常 `review` の
        // `impact.changes[].impacted_callers` には引き続き残る (情報価値を維持)。
        // (Issue: 2026-05-27-added-symbol-initial-reference)
        let affected_change_types: std::collections::HashMap<&str, &str> = change
            .affected_symbols
            .iter()
            .map(|s| (s.name.as_str(), s.change_type.as_str()))
            .collect();
        for caller in &change.impacted_callers {
            let caller_abs = if std::path::Path::new(&caller.path).is_relative() {
                std::path::Path::new(dir)
                    .join(&caller.path)
                    .to_string_lossy()
                    .to_string()
            } else {
                caller.path.clone()
            };
            let in_diff = match std::fs::canonicalize(&caller_abs) {
                Ok(canon) => changed_canonical.contains(&canon),
                Err(_) => changed_abs_strs.contains(&caller_abs),
            };
            if !in_diff {
                // breaking causal シンボル (modified / removed) だけを残す。
                // `added` 由来は hook blocking から除外する。
                let causal_syms: Vec<String> = caller
                    .symbols
                    .iter()
                    .filter(|sym| {
                        matches!(
                            affected_change_types.get(sym.as_str()).copied(),
                            Some(ct) if ct != "added"
                                && !informational_modified_api_symbols
                                    .contains(&(change.path.as_str(), sym.as_str()))
                        )
                    })
                    .cloned()
                    .collect();
                if causal_syms.is_empty() {
                    // 全 caller.symbols が added 由来 (または affected 外) → blocking しない
                    continue;
                }
                let entry = unresolved.entry(change.path.clone()).or_default();
                for sym in &causal_syms {
                    entry.changed_symbols.insert(sym.clone());
                }
                entry
                    .refs
                    .push((caller.path.clone(), caller.line, causal_syms));
            }
        }
        for caller in &change.informational_callers {
            let caller_abs = if std::path::Path::new(&caller.path).is_relative() {
                std::path::Path::new(dir)
                    .join(&caller.path)
                    .to_string_lossy()
                    .to_string()
            } else {
                caller.path.clone()
            };
            let in_diff = match std::fs::canonicalize(&caller_abs) {
                Ok(canon) => changed_canonical.contains(&canon),
                Err(_) => changed_abs_strs.contains(&caller_abs),
            };
            if !in_diff {
                let causal_syms: Vec<String> = caller
                    .symbols
                    .iter()
                    .filter(|sym| {
                        matches!(
                            affected_change_types.get(sym.as_str()).copied(),
                            Some("modified")
                        )
                    })
                    .cloned()
                    .collect();
                if causal_syms.is_empty() {
                    continue;
                }
                let entry = informational.entry(change.path.clone()).or_default();
                for sym in &causal_syms {
                    entry.changed_symbols.insert(sym.clone());
                }
                entry
                    .refs
                    .push((caller.path.clone(), caller.line, causal_syms));
            }
        }
    }

    // 空セクションは省略した compact JSON を構築
    let mut hook_obj = serde_json::Map::new();
    // has_blocking_issues: Stop hook を止めるべき重要な検出 (impacts / api / dead)
    // has_any_output: 出力すべき検出 (上記 + cochange)
    let mut has_blocking_issues = false;
    let mut has_any_output = false;

    // impacts: [{src,syms,refs:[{p,ln,s}]}]
    if !unresolved.is_empty() {
        has_blocking_issues = true;
        has_any_output = true;
        hook_obj.insert("impacts".into(), impact_groups_value(&unresolved));
    }

    // impact_info: import-only など blocking しない低信号 impact。`impacts` と分ける。
    if !informational.is_empty() {
        has_any_output = true;
        hook_obj.insert("impact_info".into(), impact_groups_value(&informational));
    }

    // cochange: [{f,w,c}] — 情報提供のみ。is_blocking にはしない
    if !result.missing_cochanges.is_empty() {
        has_any_output = true;
        let cochanges: Vec<HookCochange<'_>> = result
            .missing_cochanges
            .iter()
            .map(|cochange| HookCochange {
                f: cochange.file.as_str(),
                w: cochange.expected_with.as_str(),
                c: (cochange.confidence * 100.0).round() as u32,
            })
            .collect();
        hook_obj.insert(
            "cochange".into(),
            serde_json::to_value(cochanges).expect("hook cochange DTO should serialize"),
        );
    }

    // api: {add,rm,mod,moved,property_to_field,rm_dead,const_value} — 空でないセクションのみ
    let api = HookApi::from_api_changes(&result.api_changes);
    // api.added / api.moved / api.property_to_field / api.removed_dead / api.const_value は
    // 破壊的変更ではないため Stop hook のブロッキング対象から外し informational 扱いにする。
    // api.removed / api.modified は破壊的変更の可能性があるため従来どおり blocking。
    // const_value (値のみ変更) は `--strict-public-const-values` 指定時のみ blocking に昇格する。
    let has_api_breaking = !result.api_changes.removed.is_empty()
        || !result.api_changes.modified.is_empty()
        || (strict_const_values && !result.api_changes.const_value_changes.is_empty());
    if !api.is_empty() {
        if has_api_breaking {
            has_blocking_issues = true;
        }
        has_any_output = true;
        hook_obj.insert(
            "api".into(),
            serde_json::to_value(api).expect("hook API DTO should serialize"),
        );
    }

    // dead: [{n,f}]
    if !result.dead_symbols.is_empty() {
        has_blocking_issues = true;
        has_any_output = true;
        let dead: Vec<HookNameFile<'_>> = result
            .dead_symbols
            .iter()
            .map(|symbol| HookNameFile {
                n: symbol.name.as_str(),
                f: symbol.file.as_str(),
            })
            .collect();
        hook_obj.insert(
            "dead".into(),
            serde_json::to_value(dead).expect("hook dead-symbol DTO should serialize"),
        );
    }

    if !has_any_output {
        return HookJsonBuild {
            value: None,
            is_blocking: false,
        };
    }

    hook_obj.insert(
        "hint".into(),
        serde_json::Value::String("False positives? Run astro-sight-triage skill.".into()),
    );

    HookJsonBuild {
        value: Some(serde_json::Value::Object(hook_obj)),
        is_blocking: has_blocking_issues,
    }
}

/// --hook 時の review 出力: compact JSON を stderr に出力する。
/// blocking な検出 (impacts / api / dead) があれば exit 1、
/// cochange のみの informational な出力は exit 0 にして Stop hook を止めない。
pub(crate) fn review_hook_output(
    result: &ReviewResult,
    dir: &str,
    strict_const_values: bool,
) -> Result<()> {
    let build = build_review_hook_json(result, dir, strict_const_values);
    let Some(hook_output) = build.value else {
        return Ok(());
    };

    eprintln!("{hook_output}");
    if build.is_blocking {
        std::process::exit(1);
    }
    Ok(())
}
