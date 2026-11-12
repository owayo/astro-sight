//! 宣言の形・場所で決まらなかったシグネチャ変更の、互換と呼び出し側の追随の判定。

use super::*;

/// 分類の段階を記述順に評価し、最初に確定した段階の結果を返す。段階の順序がそのまま
/// 分類の優先順位になる (型契約の変更を互換判定や同一 diff での追随より先に確定させる等)。
/// bucket は書き換えない (格納は [`record_signature_decision`] だけが行う)。
pub(super) fn decide_signature_change(
    inputs: &DetectionInputs<'_>,
    caches: &mut ApiClosureCaches,
    site: &SignatureChangeSite<'_>,
    dependencies: Option<&super::ts_signature::DependencyChanges>,
) -> SignatureDecision {
    let dependency_status = dependencies
        .and_then(|types| types.get(&site.change.name))
        .copied()
        .unwrap_or_default();
    let &DetectionInputs {
        dir,
        base,
        ref_index,
        base_blobs,
        ..
    } = inputs;
    let &SignatureChangeSite {
        ref change,
        old_path,
        new_path,
        moved_from,
        kind,
        old_sig,
        new_sig,
        lang_id: lang_id_for_file,
        internally_closed,
    } = site;
    let name = change.name.as_str();
    // 宣言名と違う公開名 (default export / 別名 export) の利用者は宣言名の参照に現れない。
    // 参照の集合から互換・追随済みを証明する判定 (JSX だけの利用、省略可能引数の削除、
    // 同一 diff での追随) はどれも使えない。
    let public_name_differs = !name.contains('.')
        && super::js_public_name::published_names_for_diff_file(
            dir, base_blobs, old_path, new_path, caches,
        )
        .is_some_and(|names| names.may_differ(name));
    // 以降の互換判定器はすべて同じ現場情報を見る。old/new ソースは最初に必要になった
    // 判定器で 1 度だけ取得して使い回す (旧実装は判定器ごとに git show を起動していた)。
    let site = CompatibleModSite {
        dir,
        base,
        old_path,
        new_path,
        name,
        kind,
        old_sig,
        new_sig,
        lang_id: lang_id_for_file,
    };
    let sources = &mut SignatureSourceCache::with_base_blobs(base_blobs, inputs.strip_internal);
    if let Some(decision) = type_annotation_decision(&site, sources) {
        return decision;
    }
    // 既存の値変更判定を優先し、直接 TS callable の型注釈は元ソースで照合する。
    if lang_id_for_file.is_some_and(|lid| is_const_value_only_change(old_sig, new_sig, kind, lid))
        && super::ts_signature::ts_const_value_header_guard(&site, sources)
    {
        return SignatureDecision::ConstValue;
    }
    if let Some(decision) = static_const_decision(&site, sources) {
        return decision;
    }
    if let Some(decision) = python_contract_decision(&site, sources, internally_closed) {
        return decision;
    }
    if let Some(compatible) = super::rust_import_path::detect(inputs, &site, sources) {
        return SignatureDecision::Compatible(compatible);
    }
    if let Some(compat) = compatible_modification(
        ref_index,
        &site,
        sources,
        caches,
        public_name_differs,
        inputs.diff_files,
    ) {
        if dependency_status == super::ts_signature::DependencyStatus::Unknown
            && matches!(
                compat.reason.as_str(),
                "trailing_optional_params"
                    | "trailing_optional_params_removed"
                    | "optional_object_props"
            )
            && !super::ts_signature::compatible_dependencies_unchanged(
                &site,
                sources,
                &mut caches.optional_removal,
                inputs.diff_files,
                &compat.reason,
                dependencies,
            )
        {
            return SignatureDecision::Blocking(None);
        }
        return SignatureDecision::Compatible(compat);
    }
    closure_or_blocking_decision(
        inputs,
        caches,
        &site,
        sources,
        public_name_differs,
        moved_from,
    )
}

/// 型注釈だけの変更か。
pub(super) fn type_annotation_decision(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<SignatureDecision> {
    match super::type_annotation::classify_type_annotation_change(site, sources) {
        super::type_annotation::TypeAnnotationDetection::Unchanged => {
            Some(SignatureDecision::UnchangedTypeAnnotation)
        }
        super::type_annotation::TypeAnnotationDetection::AnnotationOnly => {
            Some(SignatureDecision::TypeAnnotationOnly)
        }
        super::type_annotation::TypeAnnotationDetection::NotApplicable => None,
    }
}

/// 型注釈のない静的 container は、構造に加えて葉の値も比較して整形だけを除く。
pub(super) fn static_const_decision(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<SignatureDecision> {
    match super::static_const::classify_static_const_change(site, sources) {
        super::static_const::StaticConstChange::Unchanged => {
            Some(SignatureDecision::UnchangedStaticConst)
        }
        super::static_const::StaticConstChange::ValueOnly => Some(SignatureDecision::ConstValue),
        super::static_const::StaticConstChange::NotApplicable => None,
    }
}

/// Python の TypedDict で `total=` が絡む変更は blocking な api.mod に確定させる。
/// 互換判定器や closed-in-diff 判定より**前**に置くのが要点で、「リポジトリ内の参照が
/// 同一 diff で更新済み」でも降格させないため (外部リポジトリ / 動的生成された dict は
/// 静的に追えない。Issue 2026-08-18-python-typeddict-contract-change-classification)。
///
/// **種別を確定できなくても降格させない**。分類できないことは「破壊的でない」証明では
/// ないため (`NotRequired` 同居で分類を諦めたケースでも bare フィールドの requiredness は
/// 実際に反転する)。ラベル (`contract_change`) は確定したときだけ添える。
///
/// | 検出 | `internally_closed` | 結果 |
/// |---|---|---|
/// | `NotApplicable` | false | 次の段階へ |
/// | `NotApplicable` | true | 出さない |
/// | `PotentialBreakingChange` | false | blocking |
/// | `PotentialBreakingChange` | true | 出さない |
/// | `Classified` | どちらでも | 契約ラベル付きの blocking |
pub(super) fn python_contract_decision(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    internally_closed: bool,
) -> Option<SignatureDecision> {
    match detect_python_typed_dict_total_change(site, sources) {
        // TypedDict の `total=` 変更だと**証明できなかった**ので、同一ファイル内でしか
        // 使われていないシンボルは従来どおり api.mod に出さない (呼び出し元の早期除外の続き)。
        // 証明できないケースまで出すと、`class X(Base, total=False)` のような無関係な
        // Python class を新たに blocking にしてしまう。この早期除外そのものの是非は
        // 言語横断の既存ルールなので本 Issue では触らない。
        PythonContractDetection::NotApplicable => {
            internally_closed.then_some(SignatureDecision::Omit)
        }
        PythonContractDetection::PotentialBreakingChange if internally_closed => {
            Some(SignatureDecision::Omit)
        }
        PythonContractDetection::PotentialBreakingChange => Some(SignatureDecision::Blocking(None)),
        PythonContractDetection::Classified(contract) => {
            Some(SignatureDecision::Blocking(Some(contract)))
        }
    }
}

/// props 専用の private alias からの削除だけは、型依存の変更そのものを証明の対象にする。
pub(super) fn local_props_alias_removal_decision(
    inputs: &DetectionInputs<'_>,
    caches: &mut ApiClosureCaches,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<SignatureDecision> {
    if site.kind != "function"
        || site.name.contains('.')
        || super::js_public_name::published_names_for_diff_file(
            site.dir,
            inputs.base_blobs,
            site.old_path,
            site.new_path,
            caches,
        )
        .is_some_and(|names| names.may_differ(site.name))
    {
        return None;
    }
    detect_local_alias_props_removed_compatible_mod(
        inputs.ref_index,
        site,
        sources,
        &mut caches.optional_removal,
        inputs.diff_files,
    )
    .map(SignatureDecision::Compatible)
}

/// 呼び出し側を壊さない変更を、互換判定器を記述順に試して最初に当てはまったもので返す。
/// 同じ変更に複数の判定器が当てはまるときは先の判定器の reason を採るので、順序を変えない。
pub(super) fn compatible_modification(
    ref_index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    caches: &mut ApiClosureCaches,
    public_name_differs: bool,
    diff_files: &[crate::models::impact::DiffFile],
) -> Option<CompatibleApiModification> {
    if let Some(compat) =
        detect_fixed_callback_contract_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // React component を memo / forwardRef 等の HOC でラップしただけは compatible_modified
    if !public_name_differs
        && let Some(compat) = detect_react_wrapper_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // exported object の未参照プロパティ削除も compatible_modified
    if let Some(compat) = detect_object_members_compatible_mod(site, sources) {
        return Some(compat);
    }
    if let Some(compat) = detect_type_members_compatible_mod(site, sources) {
        return Some(compat);
    }
    if !public_name_differs
        && let Some(compat) = detect_trailing_default_type_params_compatible_mod(
            ref_index,
            site,
            sources,
            &mut caches.optional_removal,
            diff_files,
        )
    {
        return Some(compat);
    }
    // TS/TSX の関数末尾へ optional/default 引数を追加しただけなら、既存呼び出しの required
    // arity は変わらないため compatible_modified として扱う。
    if let Some(compat) = detect_trailing_optional_params_compatible_mod(site, sources) {
        return Some(compat);
    }
    if !public_name_differs
        && let Some(compat) = super::ts_signature::detect_nullable_input_compatible_mod(
            ref_index,
            site,
            sources,
            &mut caches.optional_removal,
            diff_files,
        )
    {
        return Some(compat);
    }
    if !public_name_differs
        && let Some(compat) = detect_trailing_optional_params_removed_compatible_mod(
            ref_index,
            site,
            sources,
            &mut caches.optional_removal,
        )
    {
        return Some(compat);
    }
    // TS/TSX の関数コンポーネントの inline object 型の props から省略可能プロパティを消し、
    // リポジトリ内のどの JSX 要素・直接呼び出しもそのキーを渡さないと証明できたものも、
    // 呼び出し側の書き換えが要らないので compatible_modified とする (Issue #208)。
    if !public_name_differs
        && let Some(compat) = detect_optional_object_props_removed_compatible_mod(
            ref_index,
            site,
            sources,
            &mut caches.optional_removal,
            diff_files,
        )
    {
        return Some(compat);
    }
    // TS/TSX の引数 object type literal へ optional プロパティを追加しただけなら、
    // 既存呼び出しが渡す object はそのまま受理されるため compatible_modified として扱う。
    if let Some(compat) = detect_optional_object_props_compatible_mod(
        site,
        sources,
        &mut caches.react_memo,
        diff_files,
    ) {
        return Some(compat);
    }
    // 同一ファイル内で有限 literal union の値集合が old/new 同値と証明できる type alias
    // 変更は、表記・alias 経路だけの変更なので compatible_modified とする。
    // import 越し・循環・非 literal・集合差分ありは判定器が None に倒し blocking を維持する。
    if let Some(compat) = detect_equivalent_literal_union_alias_compatible_mod(site, sources) {
        return Some(compat);
    }
    // React Server Component の async 化 (async キーワード追加のみ + 全参照が JSX タグ利用)
    // は呼び出し側の書き換えが不要なため compatible_modified として扱う。
    if !public_name_differs
        && let Some(compat) =
            detect_async_jsx_component_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // 引数なし関数コンポーネントへ default 値の無い全 optional の destructured props を
    // 足しただけで、参照がすべて JSX タグ利用なら `<X />` はそのまま通るため
    // compatible_modified とする (default 値 / `?` 付きは trailing_optional_params が扱う)。
    if !public_name_differs
        && let Some(compat) =
            detect_optional_props_jsx_component_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // Python のトップレベル関数 / モジュール直下クラスのメソッドへ末尾 optional/default
    // 引数 (`*` 後の kwonly+default 含む) を追加しただけなら、既存呼び出しが壊れないため
    // compatible_modified として扱う。
    if let Some(compat) = detect_python_trailing_optional_params_compatible_mod(site, sources) {
        return Some(compat);
    }
    // Python のデフォルト値・型注釈にある純粋な隣接文字列リテラルを分割・結合しただけで、
    // 正規化後のシグネチャ全体が同一なら compatible_modified とする。f-string、escape、
    // 演算子、異種 prefix 等を含む場合は判定器が None に倒し blocking を維持する。
    detect_python_implicit_string_concat_compatible_mod(site, sources)
}

/// 全 cross-file 参照が同一 diff で追随済みなら informational、callback 本体だけの変更なら
/// その分類、どちらでもなければ blocking な api.mod にする。
pub(super) fn closure_or_blocking_decision(
    inputs: &DetectionInputs<'_>,
    caches: &mut ApiClosureCaches,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    public_name_differs: bool,
    moved_from: Option<&str>,
) -> SignatureDecision {
    caches
        .type_member_unproven_sites
        .remove(&(site.new_path.to_owned(), site.name.to_owned()));
    if !public_name_differs
        && moved_from.is_none()
        && let Some(closed) = super::python_optional_removal::prove_removal(
            inputs.ref_index,
            site,
            sources,
            &mut caches.python_optional_removal,
        )
    {
        return if closed {
            SignatureDecision::ClosedInDiff
        } else {
            SignatureDecision::Blocking(None)
        };
    }
    if !public_name_differs
        && moved_from.is_none()
        && let Some(closed) = super::ts_literal_union_removal::prove_removal(
            inputs.ref_index,
            site,
            sources,
            &mut caches.literal_union_removal,
        )
    {
        return if closed {
            SignatureDecision::ClosedInDiff
        } else {
            SignatureDecision::Unverified(ApiModificationUnverified {
                reason: ApiModificationUnverifiedReason::LiteralUnionProofFailed,
                first_unproven: None,
            })
        };
    }
    let removed_props = match super::ts_signature::removed_parameter_props(site, sources) {
        Ok(props) => props,
        Err(()) => return SignatureDecision::Blocking(None),
    };
    if removed_props.is_empty()
        && !public_name_differs
        && moved_from.is_none()
        && caches.function_handoffs.covers(inputs, site, sources)
    {
        return SignatureDecision::ClosedInDiff;
    }
    // object type literal 引数への必須プロパティ追加のみの変更なら、閉包判定へ追加証拠として
    // 渡す (呼び出し式が無変更でも共有 const の定義側が同一 diff で更新されていれば追随済み)。
    // 互換変更ではないので compatible_modified には入れず、あくまで closed 判定の入力。
    let added_required_props = detect_added_required_object_props(site, sources);
    let python_protocol =
        super::python_protocol_arg::prepare_protocol_argument_change(site, sources);
    // interface / object 型の type alias のメンバー変更は、値を作る箇所・受け取る箇所
    // ごとの証明だけで閉じる。証明に対応しない形 (generic / extends / メソッド追加など) は、
    // 参照行が変更行にあるだけでは追随済みと言えないので閉じない (Issue #89)。
    let required_members =
        match super::ts_signature::classify_required_member_addition(site, sources) {
            super::ts_required_members::RequiredMemberChange::NotApplicable => {
                match super::ts_signature::classify_member_property_removal(site, sources) {
                    super::ts_required_members::RequiredMemberChange::NotApplicable => {
                        super::ts_signature::classify_member_type_narrowing(site, sources)
                    }
                    classified => classified,
                }
            }
            classified => classified,
        };
    if matches!(
        required_members,
        super::ts_required_members::RequiredMemberChange::Unsupported
    ) {
        return SignatureDecision::Unverified(ApiModificationUnverified {
            reason: ApiModificationUnverifiedReason::UnsupportedTypeChange,
            first_unproven: None,
        });
    }
    // Python の関数・メソッドの戻り値の注釈だけの変更なら、結果の利用の書き換えを追加証拠に
    // する (Issue #251)。移動と同じ差分の変更は、呼び出し側の旧版を移動元と比べられないので除く。
    let python_return = (python_protocol.is_none() && moved_from.is_none())
        .then(|| super::python_return_usage::prepare(site, sources, inputs.base_blobs))
        .flatten();
    let arg_evidence = if !removed_props.is_empty() {
        super::ref_index::ClosureArgEvidence::TsRemovedProps(&removed_props)
    } else {
        match (
            &added_required_props,
            &python_protocol,
            &required_members,
            &python_return,
        ) {
            (Some(props), _, _, _) => {
                super::ref_index::ClosureArgEvidence::TsAddedRequiredProps(props)
            }
            (None, Some(proof), _, _) => {
                super::ref_index::ClosureArgEvidence::PythonProtocolParams(proof)
            }
            (None, None, super::ts_required_members::RequiredMemberChange::Supported(delta), _) => {
                super::ref_index::ClosureArgEvidence::TsRequiredTypeMembers(delta)
            }
            (None, None, _, Some(contract)) => {
                super::ref_index::ClosureArgEvidence::PythonReturnContract(contract)
            }
            _ => super::ref_index::ClosureArgEvidence::None,
        }
    };
    // 真偽判定の不変性は関数のシグネチャ変更だけの証拠。値から関数への種類変更は除く。
    // ソースは互換判定器と共有し、候補ごとに一度だけ旧新の宣言を確かめる。
    let js_direct_function_change = site.kind == "function"
        && !site.name.contains('.')
        && site
            .lang_in(&[
                crate::language::LangId::Javascript,
                crate::language::LangId::Typescript,
                crate::language::LangId::Tsx,
            ])
            .and_then(|lang| sources.get(site)?.parse_pair(lang))
            .is_some_and(|(old, new)| {
                let Some(pair) = sources.get(site) else {
                    return false;
                };
                crate::engine::js_occurrence::unique_direct_function(
                    old.root_node(),
                    &pair.old,
                    site.name,
                ) && crate::engine::js_occurrence::unique_direct_function(
                    new.root_node(),
                    &pair.new,
                    site.name,
                )
            });
    let rust_module_function_change = (site.kind == "function"
        && !site.name.contains('.')
        && site.lang_id == Some(crate::language::LangId::Rust))
    .then(|| sources.get(site))
    .flatten()
    .and_then(|pair| {
        let (old, new) = pair.parse_pair(crate::language::LangId::Rust)?;
        super::rust_ref_sites::ModuleFunctions::from_root(old.root_node(), &pair.old)?
            .prove(site.name, site.old_sig)?;
        super::rust_ref_sites::ModuleFunctions::from_root(new.root_node(), &pair.new)?
            .prove(site.name, site.new_sig)
    });
    // 全 cross-file 参照が同一 diff 内で追随済みなら informational
    if is_modified_closed_in_diff(
        ModifiedClosureInput {
            index: inputs.ref_index,
            dir: inputs.dir,
            name: site.name,
            kind: site.kind,
            base: inputs.base,
            target_new_path: site.new_path,
            diff_files: inputs.diff_files,
            arg_evidence,
            public_name_differs,
            js_direct_function_change,
            rust_module_function_change,
            moved_from,
        },
        caches,
    ) {
        SignatureDecision::ClosedInDiff
    } else if detect_callback_body_only_change(site, sources) {
        SignatureDecision::CallbackBodyOnly
    } else if matches!(
        required_members,
        super::ts_required_members::RequiredMemberChange::Supported(_)
    ) {
        SignatureDecision::Unverified(ApiModificationUnverified {
            reason: ApiModificationUnverifiedReason::TypeMemberProofFailed,
            first_unproven: caches
                .type_member_unproven_sites
                .remove(&(site.new_path.to_owned(), site.name.to_owned())),
        })
    } else if site
        .lang_in(&[
            crate::language::LangId::Typescript,
            crate::language::LangId::Tsx,
        ])
        .is_some()
        && matches!(site.kind, "type" | "trait" | "interface")
    {
        SignatureDecision::Unverified(ApiModificationUnverified {
            reason: ApiModificationUnverifiedReason::UnsupportedTypeChange,
            first_unproven: None,
        })
    } else {
        SignatureDecision::Blocking(None)
    }
}
