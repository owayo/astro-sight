//! 判定したシグネチャ変更を bucket へ入れる入口。

use super::*;

/// 分類結果を対応する bucket へ格納する。bucket を書き換えるのはここだけ。
/// 互換・追随の証明を完了できなかった変更 1 件を、その理由 (と最初に証明できなかった場所) を
/// 添えて blocking の api.mod として記録する。記録は [`record_signature_decision`] に任せる。
pub(super) fn record_unverified_modification(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    change: ApiSymbolChange,
    unverified: ApiModificationUnverified,
) {
    record_signature_decision(
        inputs,
        state,
        change,
        SignatureDecision::Unverified(unverified),
    );
}

pub(super) fn record_signature_decision(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    mut change: ApiSymbolChange,
    decision: SignatureDecision,
) {
    let buckets = &mut *state.buckets;
    match decision {
        SignatureDecision::Omit => {}
        SignatureDecision::UnchangedTypeAnnotation => {
            buckets.unchanged_type_annotations.push(ApiSymbol {
                inline_module: change.inline_module,
                name: change.name,
                kind: change.kind,
                file: change.file,
                refs_internal: 0,
                cfg_availability: None,
            });
        }
        SignatureDecision::TypeAnnotationOnly => {
            buckets
                .type_annotation_changes
                .push(crate::models::review::TypeAnnotationChange {
                    change,
                    reason: crate::models::review::TypeAnnotationChangeReason::TypeAnnotationOnly,
                    compatibility: crate::models::review::TypeAnnotationCompatibility::Unverified,
                });
        }
        SignatureDecision::ConstValue => buckets.const_value_changes.push(change),
        SignatureDecision::UnchangedStaticConst => {
            buckets.unchanged_static_consts.push(ApiSymbol {
                inline_module: change.inline_module,
                name: change.name,
                kind: change.kind,
                file: change.file,
                refs_internal: 0,
                cfg_availability: None,
            });
        }
        SignatureDecision::Compatible(mut compat)
        | SignatureDecision::MovedConstAddition(mut compat) => {
            // 互換の判定器は名前で作るので、変更の inline module の経路を引き継ぐ。
            if compat.inline_module.is_none() {
                compat.inline_module = change.inline_module;
            }
            buckets.compatible_modified.push(compat)
        }
        SignatureDecision::ClosedInDiff => buckets.modified_closed_in_diff.push(change),
        SignatureDecision::CallbackBodyOnly => {
            buckets
                .callback_body_changes
                .push(crate::models::review::CallbackBodyChange {
                    change,
                    reason: crate::models::review::CallbackBodyChangeReason::CallbackBodyChange,
                });
        }
        SignatureDecision::Blocking(contract) => {
            record_blocking_modification(inputs, state, change, contract);
        }
        SignatureDecision::Unverified(diagnostic) => {
            change.unverified = Some(diagnostic);
            record_blocking_modification(inputs, state, change, None);
        }
    }
}

pub(super) fn record_blocking_modification(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    mut change: ApiSymbolChange,
    contract: Option<crate::models::review::ApiContractChange>,
) {
    if let Some(contract) = contract {
        change.contract_change = Some(contract);
    }
    change.no_resolved_internal_callers = has_no_resolved_internal_callers(
        inputs.ref_index,
        inputs.dir,
        &change.name,
        state.closure_caches,
    );
    state.buckets.modified.push(change);
}
