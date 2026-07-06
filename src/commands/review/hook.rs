use anyhow::Result;

use crate::models::review::ReviewResult;

/// `--hook` の出力判定結果。
/// - `value`: stderr に書き出す JSON (何もなければ None)
/// - `is_blocking`: exit 1 にして Stop hook を止めるべきか。cochange だけは informational
///   として block しない (レポート 2026-04-11-cochange-new-repo-initial-commit-noise.md の提案)
pub(crate) struct HookJsonBuild {
    pub value: Option<serde_json::Value>,
    pub is_blocking: bool,
}

pub(crate) fn build_review_hook_json(
    result: &ReviewResult,
    dir: &str,
    strict_const_values: bool,
) -> HookJsonBuild {
    #[derive(Default)]
    struct HookImpactGroup {
        changed_symbols: std::collections::BTreeSet<String>,
        refs: Vec<(String, usize, Vec<String>)>,
    }

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
        let impacts: Vec<serde_json::Value> = unresolved
            .iter()
            .map(|(changed_path, group)| {
                let refs: Vec<serde_json::Value> = group
                    .refs
                    .iter()
                    .map(|(p, ln, s)| {
                        let mut r = serde_json::Map::new();
                        r.insert("p".into(), serde_json::Value::String(p.clone()));
                        r.insert("ln".into(), serde_json::json!(*ln));
                        if !s.is_empty() {
                            r.insert(
                                "s".into(),
                                serde_json::Value::Array(
                                    s.iter()
                                        .map(|v| serde_json::Value::String(v.clone()))
                                        .collect(),
                                ),
                            );
                        }
                        serde_json::Value::Object(r)
                    })
                    .collect();
                serde_json::json!({
                    "src": changed_path,
                    "syms": group.changed_symbols.iter().collect::<Vec<_>>(),
                    "refs": refs,
                })
            })
            .collect();
        hook_obj.insert("impacts".into(), serde_json::Value::Array(impacts));
    }

    // impact_info: import-only など blocking しない低信号 impact。`impacts` と分ける。
    if !informational.is_empty() {
        has_any_output = true;
        let impacts: Vec<serde_json::Value> = informational
            .iter()
            .map(|(changed_path, group)| {
                let refs: Vec<serde_json::Value> = group
                    .refs
                    .iter()
                    .map(|(p, ln, s)| {
                        let mut r = serde_json::Map::new();
                        r.insert("p".into(), serde_json::Value::String(p.clone()));
                        r.insert("ln".into(), serde_json::json!(*ln));
                        if !s.is_empty() {
                            r.insert(
                                "s".into(),
                                serde_json::Value::Array(
                                    s.iter()
                                        .map(|v| serde_json::Value::String(v.clone()))
                                        .collect(),
                                ),
                            );
                        }
                        serde_json::Value::Object(r)
                    })
                    .collect();
                serde_json::json!({
                    "src": changed_path,
                    "syms": group.changed_symbols.iter().collect::<Vec<_>>(),
                    "refs": refs,
                })
            })
            .collect();
        hook_obj.insert("impact_info".into(), serde_json::Value::Array(impacts));
    }

    // cochange: [{f,w,c}] — 情報提供のみ。is_blocking にはしない
    if !result.missing_cochanges.is_empty() {
        has_any_output = true;
        let cochanges: Vec<serde_json::Value> = result
            .missing_cochanges
            .iter()
            .map(|mc| {
                serde_json::json!({
                    "f": mc.file,
                    "w": mc.expected_with,
                    "c": (mc.confidence * 100.0).round() as u32,
                })
            })
            .collect();
        hook_obj.insert("cochange".into(), serde_json::Value::Array(cochanges));
    }

    // api: {add,rm,mod,moved,property_to_field,rm_dead,const_value} — 空でないセクションのみ
    let has_api_changes = !result.api_changes.added.is_empty()
        || !result.api_changes.removed.is_empty()
        || !result.api_changes.modified.is_empty()
        || !result.api_changes.moved.is_empty()
        || !result.api_changes.property_to_field.is_empty()
        || !result.api_changes.removed_dead.is_empty()
        || !result.api_changes.modified_closed_in_diff.is_empty()
        || !result.api_changes.const_value_changes.is_empty()
        || !result.api_changes.compatible_modified.is_empty();
    // api.added / api.moved / api.property_to_field / api.removed_dead / api.const_value は
    // 破壊的変更ではないため Stop hook のブロッキング対象から外し informational 扱いにする。
    // api.removed / api.modified は破壊的変更の可能性があるため従来どおり blocking。
    // const_value (値のみ変更) は `--strict-public-const-values` 指定時のみ blocking に昇格する。
    let has_api_breaking = !result.api_changes.removed.is_empty()
        || !result.api_changes.modified.is_empty()
        || (strict_const_values && !result.api_changes.const_value_changes.is_empty());
    if has_api_changes {
        if has_api_breaking {
            has_blocking_issues = true;
        }
        has_any_output = true;
        let mut api = serde_json::Map::new();
        if !result.api_changes.added.is_empty() {
            api.insert(
                "add".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .added
                        .iter()
                        .map(|s| serde_json::json!({"n": s.name, "f": s.file}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.removed.is_empty() {
            api.insert(
                "rm".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .removed
                        .iter()
                        .map(|s| serde_json::json!({"n": s.name, "f": s.file}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.modified.is_empty() {
            api.insert(
                "mod".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .modified
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file}))
                        .collect(),
                ),
            );
        }
        // mod_closed: 全 cross-file 参照が同一 diff 内で追随済みの api.mod。informational
        // (has_api_breaking に含めないため stop hook をブロックしない)。
        if !result.api_changes.modified_closed_in_diff.is_empty() {
            api.insert(
                "mod_closed".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .modified_closed_in_diff
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file}))
                        .collect(),
                ),
            );
        }
        // const_value: const / 非 mut static / export const の値のみ変更。shape (名前・型・
        // visibility) は不変でコンパイル互換性を壊さないため informational
        // (デフォルト非 blocking、`--strict-public-const-values` 指定時のみ blocking 昇格)。
        if !result.api_changes.const_value_changes.is_empty() {
            api.insert(
                "const_value".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .const_value_changes
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file}))
                        .collect(),
                ),
            );
        }
        // mod_compat: signature 文字列は変わったが公開契約が維持される互換 api.mod
        // (React HOC ラップ / 未参照プロパティ削除)。informational (非 blocking)。reason 付き。
        if !result.api_changes.compatible_modified.is_empty() {
            api.insert(
                "mod_compat".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .compatible_modified
                        .iter()
                        .map(|m| serde_json::json!({"n": m.name, "f": m.file, "reason": m.reason}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.moved.is_empty() {
            api.insert(
                "moved".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .moved
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "n": m.name,
                                "from": m.from,
                                "to": m.to,
                            })
                        })
                        .collect(),
                ),
            );
        }
        if !result.api_changes.removed_dead.is_empty() {
            api.insert(
                "rm_dead".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .removed_dead
                        .iter()
                        .map(|s| serde_json::json!({"n": s.name, "f": s.file}))
                        .collect(),
                ),
            );
        }
        if !result.api_changes.property_to_field.is_empty() {
            api.insert(
                "property_to_field".into(),
                serde_json::Value::Array(
                    result
                        .api_changes
                        .property_to_field
                        .iter()
                        .map(|p| serde_json::json!({"n": p.name, "f": p.file}))
                        .collect(),
                ),
            );
        }
        hook_obj.insert("api".into(), serde_json::Value::Object(api));
    }

    // dead: [{n,f}]
    if !result.dead_symbols.is_empty() {
        has_blocking_issues = true;
        has_any_output = true;
        let dead: Vec<serde_json::Value> = result
            .dead_symbols
            .iter()
            .map(|ds| serde_json::json!({"n": ds.name, "f": ds.file}))
            .collect();
        hook_obj.insert("dead".into(), serde_json::Value::Array(dead));
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
