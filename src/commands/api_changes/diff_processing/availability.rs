//! Rust の項目の有る設定 (`#[cfg]`・module の置き場・再公開の名前) の変化の比較と記録。

use super::*;

/// Rust の変更ファイルで、旧新の両方に宣言のある項目の有る設定を比べる (Issue #291)。項目の API の
/// signature は項目自身の `#[cfg]` を含まないので、signature の比較とは別に、同じキーの宣言
/// (`#[cfg]` の分岐をまとめて) の有る条件を比べる。旧版で有った設定の一部から消えた項目は削除と
/// 同じ区分の候補にし、無かった設定に増えた項目は追加と同じ絞り込みを通して追加の候補にする
/// (別の条件への付け替えは両方)。比べられなければ blocking の変更にする。
pub(super) fn collect_rust_availability_changes(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    pairing: &RustPairing,
    declarations: &RustDeclarationPair,
) {
    use super::rust_availability::{AvailabilityChange, compare, describe};
    let &DetectionInputs {
        dir,
        base_blobs,
        diff_new_paths,
        ref_index,
        ..
    } = inputs;
    if is_binary_only_rust_crate(dir, &df.new_path) {
        return;
    }
    for (olds, news) in &pairing.groups {
        // module の宣言の条件の変化で比べ直した項目は、置き場の条件を含めて比べてある (Issue #305)。
        fn described<'a>(
            indices: &[usize],
            symbols: &'a [(String, String, String)],
            contexts: &'a [RustDeclarationContext],
        ) -> Vec<(&'a RustDeclarationContext, &'a str, &'a str)> {
            indices
                .iter()
                .map(|&k| (&contexts[k], symbols[k].0.as_str(), symbols[k].1.as_str()))
                .collect()
        }
        if inputs.module_availability.covers_availability(
            df,
            &described(olds, facts.old_syms, &declarations.old),
            &described(news, facts.new_syms, &declarations.new),
        ) {
            continue;
        }
        let old: Vec<&RustDeclarationContext> =
            olds.iter().map(|&i| &declarations.old[i]).collect();
        let new: Vec<&RustDeclarationContext> =
            news.iter().map(|&j| &declarations.new[j]).collect();
        let plain = compare(&old, &new);
        // 宣言の条件が同じでも、別のファイルへの rename なら、移動元の旧い置き場と移動先の新しい置き場の条件を
        // 足して比べ直す (Issue #316)。同じファイルの置き場の変化は、module の比べ直し (#305) が比べる。
        if plain == AvailabilityChange::Same && df.old_path == df.new_path {
            continue;
        }
        let decision =
            inputs
                .placement_availability
                .decide(&df.old_path, &old, &df.new_path, &new, plain);
        let change = decision.change;
        if change == AvailabilityChange::Same {
            continue;
        }
        let (Some(&first_old), Some(&first_new)) = (olds.first(), news.first()) else {
            continue;
        };
        let (name, kind, old_sig) = &facts.old_syms[first_old];
        let new_sig = &facts.new_syms[first_new].2;
        let candidate = ApiSymbolCandidate {
            inline_module: declarations.new_inline_module(first_new),
            name: name.clone(),
            kind: kind.clone(),
            file: df.new_path.clone(),
            signature: new_sig.clone(),
        };
        // inherent impl の項目は impl の対象の型で決める (Issue #315)。
        let removable = !rust_declaration_outside_public_api_surface(
            inputs.rust_crates,
            state.rust_public,
            dir,
            base_blobs,
            true,
            &df.old_path,
            name,
            Some(&declarations.old[first_old]),
        );
        let (availability, gained_availability) = decision.displays(|| describe(&old, &new));
        let item = AvailabilityItem {
            candidate,
            old_signature: old_sig.clone(),
            availability,
            gained_availability,
            removable,
        };
        record_availability_change(inputs, state, change, item, |state| {
            let public = !rust_declaration_outside_public_api_surface(
                inputs.rust_crates,
                state.rust_public,
                dir,
                base_blobs,
                false,
                &df.new_path,
                name,
                Some(&declarations.new[first_new]),
            );
            (
                public,
                public
                    && !is_internally_connected(facts.in_file_callees, name)
                    && !is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths),
            )
        });
    }
}

/// 有る設定の比較 (Issue #291) で振り分ける項目。
pub(super) struct AvailabilityItem {
    /// 新しい版の項目 (削除・追加の候補にする)。
    pub(super) candidate: ApiSymbolCandidate,
    /// 旧い版の signature (比べられなかったときの変更に添える)。
    pub(super) old_signature: String,
    /// 旧新の有る条件の表示。
    pub(super) availability: crate::models::review::CfgAvailability,
    /// 増える設定の表示が `availability` と違えば、その表示 (置き場の条件を足して、消える設定と増える設定を
    /// 別の比較で見つけたとき。Issue #316)。
    pub(super) gained_availability: Option<crate::models::review::CfgAvailability>,
    /// 旧い版で公開 API 面にあり、消える設定を削除と同じ区分の候補にしてよいか。
    pub(super) removable: bool,
}

/// 有る設定の比較の結果 (Issue #291) を、変更ファイルと移動で共通の規則で振り分ける。消える設定が
/// あれば削除と同じ区分の候補、増える設定があれば (`addable` なら) 追加の候補、比べられなければ
/// blocking の変更にする。
pub(super) fn record_availability_change(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    change: super::rust_availability::AvailabilityChange,
    item: AvailabilityItem,
    addable: impl FnOnce(&mut DetectionState<'_>) -> (bool, bool),
) {
    use super::rust_availability::AvailabilityChange;
    let AvailabilityItem {
        candidate,
        old_signature,
        availability,
        gained_availability,
        removable,
    } = item;
    match change {
        AvailabilityChange::Same => {}
        AvailabilityChange::Unverified => {
            if !removable {
                return;
            }
            record_unverified_modification(
                inputs,
                state,
                candidate.into_modified(old_signature),
                ApiModificationUnverified {
                    reason: ApiModificationUnverifiedReason::RustCfgAvailabilityUnverified,
                    first_unproven: None,
                },
            );
        }
        AvailabilityChange::Changed { lost, gained } => {
            if lost && removable {
                state
                    .buckets
                    .cfg_removed
                    .push((candidate.clone(), availability.clone()));
            }
            if gained {
                let (public, unused_in_diff) = addable(state);
                if public {
                    state
                        .buckets
                        .added_reference_candidates
                        .push(candidate.clone());
                }
                if unused_in_diff {
                    state
                        .buckets
                        .cfg_added
                        .push((candidate, gained_availability.unwrap_or(availability)));
                }
            }
        }
    }
}

/// module の宣言の条件が変わった module の項目の比べ直しの結果 (Issue #305) を、ファイルごとの比較と
/// 同じ区分に振り分ける。ファイルごとの比較が同じ項目を出していれば重ねない。
pub(crate) fn record_module_availability(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
) {
    use super::rust_availability::AvailabilityChange;
    use super::rust_module_availability::Outcome;
    // 同じ項目の同じ条件の報告は重ねない (同じファイルを 2 つの module に使えば、条件の違う報告は別に
    // 残す)。
    let identity = |candidate: &ApiSymbolCandidate,
                    availability: &crate::models::review::CfgAvailability| {
        format!(
            "{}\u{0}{:?}\u{0}{}\u{0}{}\u{0}{:?}",
            candidate.file, candidate.inline_module, candidate.name, candidate.kind, availability
        )
    };
    let mut reported_removed: std::collections::HashSet<String> = state
        .buckets
        .cfg_removed
        .iter()
        .map(|(candidate, availability)| identity(candidate, availability))
        .collect();
    let mut reported_added: std::collections::HashSet<String> = state
        .buckets
        .cfg_added
        .iter()
        .map(|(candidate, availability)| identity(candidate, availability))
        .collect();
    let mut callees: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for outcome in inputs.module_availability.outcomes() {
        match outcome {
            Outcome::Availability {
                change,
                candidate,
                old_signature,
                availability,
                removable,
            } => {
                let reported = identity(candidate, availability);
                let change = match *change {
                    AvailabilityChange::Changed { lost, gained } => {
                        let lost = lost && reported_removed.insert(reported.clone());
                        let gained = gained && reported_added.insert(reported);
                        if !lost && !gained {
                            continue;
                        }
                        AvailabilityChange::Changed { lost, gained }
                    }
                    other => other,
                };
                let item = AvailabilityItem {
                    candidate: candidate.clone(),
                    old_signature: old_signature.clone(),
                    availability: availability.clone(),
                    gained_availability: None,
                    removable: *removable,
                };
                let name = candidate.name.clone();
                let file = candidate.file.clone();
                record_availability_change(inputs, state, change, item, |_| {
                    let callees = callees.entry(file.clone()).or_insert_with(|| {
                        super::prepare::extract_new_file_facts(
                            inputs.dir,
                            &file,
                            inputs.strip_internal,
                        )
                        .callees
                    });
                    (
                        true,
                        !is_internally_connected(callees, &name)
                            && !is_used_in_diff_paths(
                                inputs.ref_index,
                                inputs.dir,
                                &name,
                                &file,
                                inputs.diff_new_paths,
                            ),
                    )
                });
            }
            Outcome::Ambiguous {
                candidate,
                old_signature,
            } => {
                record_unverified_modification(
                    inputs,
                    state,
                    candidate.clone().into_modified(old_signature.clone()),
                    ApiModificationUnverified {
                        reason: ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous,
                        first_unproven: None,
                    },
                );
            }
        }
    }
}

/// 再公開した名前の比べ直しの結果 (Issue #306・#318) を振り分ける: 有る設定の変化は #291 と同じ区分、
/// 名前の削除・追加はふつうの削除・追加 (移動の相殺にかけない)、比べられなければ blocking の変更。
pub(crate) fn record_reexport_names(inputs: &DetectionInputs<'_>, state: &mut DetectionState<'_>) {
    use super::rust_availability::AvailabilityChange;
    use super::rust_reexport_names::NameOutcome;
    // 追加は #291 と同じ絞り込み: 名前を作るファイルの中から呼ぶ名前と、差分のほかのファイルで使う名前は
    // 除く。
    let mut callees: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    let mut addable = |candidate: &ApiSymbolCandidate| {
        let in_file = callees.entry(candidate.file.clone()).or_insert_with(|| {
            super::prepare::extract_new_file_facts(
                inputs.dir,
                &candidate.file,
                inputs.strip_internal,
            )
            .callees
        });
        !is_internally_connected(in_file, &candidate.name)
            && !is_used_in_diff_paths(
                inputs.ref_index,
                inputs.dir,
                &candidate.name,
                &candidate.file,
                inputs.diff_new_paths,
            )
    };
    for outcome in inputs.reexport_names.outcomes() {
        match outcome {
            NameOutcome::Availability {
                candidate,
                lost,
                gained,
                availability,
            } => {
                let item = AvailabilityItem {
                    candidate: candidate.clone(),
                    old_signature: candidate.signature.clone(),
                    availability: availability.clone(),
                    gained_availability: None,
                    removable: true,
                };
                let change = AvailabilityChange::Changed {
                    lost: *lost,
                    gained: *gained,
                };
                record_availability_change(inputs, state, change, item, |_| {
                    (true, addable(candidate))
                });
            }
            NameOutcome::Removed(candidate, targets) => {
                state
                    .buckets
                    .reexport_removed
                    .push((candidate.clone(), targets.clone()));
            }
            NameOutcome::Added(candidate, targets) => {
                state
                    .buckets
                    .added_reference_candidates
                    .push(candidate.clone());
                if addable(candidate) {
                    state
                        .buckets
                        .reexport_added
                        .push((candidate.clone(), targets.clone()));
                }
            }
            NameOutcome::Unverified {
                candidate,
                old_signature,
            } => {
                record_unverified_modification(
                    inputs,
                    state,
                    candidate.clone().into_modified(old_signature.clone()),
                    ApiModificationUnverified {
                        reason: ApiModificationUnverifiedReason::RustCfgAvailabilityUnverified,
                        first_unproven: None,
                    },
                );
            }
        }
    }
}

/// 内容の同じ rename (hunk の無い変更) をした Rust のファイルの公開の項目の有る設定を、旧い置き場と新しい
/// 置き場の条件で比べる (Issue #316)。宣言自身の条件は旧新で同じなので、置き場の条件が違うときだけ変わる。
/// module の比べ直し (#305・#314) が扱った項目は飛ばす。
pub(crate) fn compare_renamed_availability(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    renames: &[crate::models::impact::DiffFile],
) {
    use super::rust_availability::{AvailabilityChange, describe};
    let rust = |path: &str| {
        crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok()
            == Some(crate::language::LangId::Rust)
    };
    for df in renames {
        if df.old_path == df.new_path
            || !rust(&df.old_path)
            || !rust(&df.new_path)
            || is_binary_only_rust_crate(inputs.dir, &df.new_path)
        {
            continue;
        }
        let Some(source) = load_new_source(inputs.dir, &df.new_path).map(|source| source.to_vec())
        else {
            continue;
        };
        let Some((symbols, Some(declarations))) =
            extract_deleted_exported_facts(&df.new_path, &source, inputs.strip_internal)
        else {
            continue;
        };
        // 同じ場所の `#[cfg]` の分岐 (同じ inline module・impl の対象・名前・種類) をまとめる。
        type Key = (Option<String>, Vec<String>, String, String);
        let mut groups: std::collections::BTreeMap<Key, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, ((name, kind, _), context)) in symbols.iter().zip(&declarations).enumerate() {
            groups
                .entry((
                    inline_module_of(context),
                    context.owner.clone(),
                    name.clone(),
                    kind.clone(),
                ))
                .or_default()
                .push(index);
        }
        for ((inline_module, _, name, kind), indices) in groups {
            let side: Vec<&RustDeclarationContext> =
                indices.iter().map(|&index| &declarations[index]).collect();
            let described: Vec<(&RustDeclarationContext, &str, &str)> = indices
                .iter()
                .map(|&index| {
                    (
                        &declarations[index],
                        symbols[index].0.as_str(),
                        symbols[index].1.as_str(),
                    )
                })
                .collect();
            if inputs
                .module_availability
                .covers_availability(df, &described, &described)
            {
                continue;
            }
            let decision = inputs.placement_availability.decide(
                &df.old_path,
                &side,
                &df.new_path,
                &side,
                AvailabilityChange::Same,
            );
            let change = decision.change;
            if change == AvailabilityChange::Same {
                continue;
            }
            let (availability, gained_availability) = decision.displays(|| describe(&side, &side));
            let first = indices[0];
            let signature = symbols[first].2.clone();
            let removable = !rust_declaration_outside_public_api_surface(
                inputs.rust_crates,
                state.rust_public,
                inputs.dir,
                inputs.base_blobs,
                true,
                &df.old_path,
                &name,
                Some(&declarations[first]),
            );
            let item = AvailabilityItem {
                candidate: ApiSymbolCandidate {
                    inline_module,
                    name: name.clone(),
                    kind,
                    file: df.new_path.clone(),
                    signature: signature.clone(),
                },
                old_signature: signature,
                availability,
                gained_availability,
                removable,
            };
            record_availability_change(inputs, state, change, item, |state| {
                let public = !rust_declaration_outside_public_api_surface(
                    inputs.rust_crates,
                    state.rust_public,
                    inputs.dir,
                    inputs.base_blobs,
                    false,
                    &df.new_path,
                    &name,
                    Some(&declarations[first]),
                );
                (
                    public,
                    public
                        && !is_used_in_diff_paths(
                            inputs.ref_index,
                            inputs.dir,
                            &name,
                            &df.new_path,
                            inputs.diff_new_paths,
                        ),
                )
            });
        }
    }
}
