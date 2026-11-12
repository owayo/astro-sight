use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use serde::Serialize;

use crate::commands::DiffCallerResolution;
use crate::models::impact::ImpactedCaller;
use crate::models::review::{ApiChanges, ReviewResult};
use crate::models::truncation::TruncationReason;

/// `--hook` の出力判定結果。
/// - `value`: stderr に書き出す JSON (何もなければ None)
/// - `is_blocking`: exit 1 にして Stop hook を止めるべきか。cochange だけは informational
///   として block しない (レポート 2026-04-11-cochange-new-repo-initial-commit-noise.md の提案)
pub(crate) struct HookJsonBuild {
    pub value: Option<serde_json::Value>,
    pub is_blocking: bool,
}

/// hook の exit 1 を発生させたカテゴリ。情報提供の同居を原因と誤認しないため、
/// 最終フィルタ後の判定と説明を同じ集合から生成する。列挙順を出力順として固定する。
#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
enum HookBlockingCategory {
    #[serde(rename = "api.diff_base_mismatch")]
    ApiDiffBaseMismatch,
    /// Rust の束縛の変化を名指し得るファイルを探し切れなかった (Issue #300)。
    #[serde(rename = "api.rust_dependents_incomplete")]
    ApiRustDependentsIncomplete,
    /// module の宣言の条件が変わった Rust の module の公開の項目を探し切れなかった (Issue #305)。
    #[serde(rename = "api.rust_module_availability_incomplete")]
    ApiRustModuleAvailabilityIncomplete,
    /// 変わった `use` が再公開する名前を探し切れなかった (Issue #306・#318)。
    #[serde(rename = "api.rust_reexports_incomplete")]
    ApiRustReexportsIncomplete,
    #[serde(rename = "impacts")]
    Impacts,
    #[serde(rename = "api.rm")]
    ApiRm,
    #[serde(rename = "api.mod")]
    ApiMod,
    /// strict 指定時の `api.const_value` バケット。
    #[serde(rename = "api.const_value")]
    ApiConstValue,
    #[serde(rename = "api.callback_body")]
    ApiCallbackBody,
    #[serde(rename = "policy.type_annotation")]
    PolicyTypeAnnotation,
    #[serde(rename = "dead")]
    Dead,
}

impl HookBlockingCategory {
    /// 打ち切りの理由のうち、API の判定を確かめられないので hook を止めるもの。ほかの理由は申告だけ
    /// にする。理由を足したら、止めるかどうかをここで決める (`_` を置かない)。
    fn for_truncation(reason: TruncationReason) -> Option<Self> {
        match reason {
            TruncationReason::DiffBaseMismatch => Some(Self::ApiDiffBaseMismatch),
            TruncationReason::RustDependentsIncomplete => Some(Self::ApiRustDependentsIncomplete),
            TruncationReason::RustModuleAvailabilityIncomplete => {
                Some(Self::ApiRustModuleAvailabilityIncomplete)
            }
            TruncationReason::RustReexportsIncomplete => Some(Self::ApiRustReexportsIncomplete),
            TruncationReason::ParseErrorRegion
            | TruncationReason::UntrackedFileTooLarge
            | TruncationReason::UnanalyzableSource
            | TruncationReason::StagedSnapshotUnavailable
            | TruncationReason::SourceAnalysisFailed
            | TruncationReason::TsExtensionProofUnavailable
            | TruncationReason::GeneratedDetectionUnavailable
            | TruncationReason::TsStripInternalUnavailable => None,
        }
    }
}

#[derive(Default)]
struct HookImpactGroup {
    changed_symbols: std::collections::BTreeSet<String>,
    refs: Vec<(String, usize, Vec<String>)>,
}

#[derive(Default)]
struct HookChangeFlags {
    has_non_added: bool,
    has_modified: bool,
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
struct HookChangeEvidence<'a> {
    f: &'a str,
    n: &'a str,
    k: &'a str,
    change_evidence: &'a crate::models::change_evidence::ChangeEvidence,
}

#[derive(Serialize)]
struct HookNameFile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    change_evidence: Option<&'a crate::models::change_evidence::ChangeEvidence>,
    n: &'a str,
    /// Rust の項目が inline module の中にあれば、ファイルの中のその経路 (Issue #282)。
    #[serde(rename = "m", skip_serializing_if = "Option::is_none")]
    inline_module: Option<&'a str>,
    f: &'a str,
    /// Rust の項目の有る設定の一部から消えたときの旧新の条件 (rm・rm_dead。Issue #291)。
    #[serde(skip_serializing_if = "Option::is_none")]
    cfg: Option<&'a crate::models::review::CfgAvailability>,
}

/// api.add 用 DTO。`HookNameFile` と分けるのは、同一ファイル内参照数を持つのが added だけで、
/// 他の api バケット (rm / mod / mod_closed / const_value / rm_dead) は算出していないため
/// (`HookDeadSymbol` を宣言行のために分けているのと同じ理由)。
#[derive(Serialize)]
struct HookAddedSymbol<'a> {
    n: &'a str,
    /// Rust の項目が inline module の中にあれば、ファイルの中のその経路 (Issue #282)。
    #[serde(rename = "m", skip_serializing_if = "Option::is_none")]
    inline_module: Option<&'a str>,
    f: &'a str,
    /// 同一ファイル内の実利用参照数 (定義 / import・re-export 行を除く)。
    /// `add_scope` は「同一 diff の他ファイルから参照されていない」抽出範囲しか表せず、
    /// 同一ファイル内の型注釈参照などは読み取れないため、シンボル単位で補う
    /// (Issue 2026-08-04-review-add-scope-naming)。0 のときは省略する。
    #[serde(
        rename = "ri",
        skip_serializing_if = "crate::models::review::is_zero_usize"
    )]
    refs_internal: usize,
    /// Rust の項目が無かった設定に増えたときの旧新の条件 (Issue #291)。
    #[serde(skip_serializing_if = "Option::is_none")]
    cfg: Option<&'a crate::models::review::CfgAvailability>,
}

/// api.mod 用 DTO。`HookNameFile` と分けるのは、「解決できた呼び出し参照ゼロ」フラグを
/// 持つのが blocking な modified だけで、他バケット (rm / mod_closed / const_value / rm_dead)
/// では算出していないため (`HookAddedSymbol` の `ri` と同じ理由)。
#[derive(Serialize)]
struct HookModifiedSymbol<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    change_evidence: Option<&'a crate::models::change_evidence::ChangeEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference_evidence: Option<&'a crate::models::reference_evidence::ReferenceEvidence>,
    n: &'a str,
    /// Rust の項目が inline module の中にあれば、ファイルの中のその経路 (Issue #282)。
    #[serde(rename = "m", skip_serializing_if = "Option::is_none")]
    inline_module: Option<&'a str>,
    f: &'a str,
    /// リポジトリ内に解決できた呼び出し参照が 1 件も無い。`api.mod` は「未更新の呼び出し側が
    /// あるかもしれない」警告だが、呼び出し参照 0 件では更新すべき呼び出し側が存在せず、
    /// 出力だけではその区別がつかない (Issue 2026-08-05-api-mod-callers-updated-indirectly)。
    /// 分類も blocking も変えず、トリアージが呼び出し側を探す段階を省けるようにするだけ。
    /// false のときは省略する。
    #[serde(
        rename = "no_callers",
        skip_serializing_if = "crate::models::review::is_false"
    )]
    no_resolved_internal_callers: bool,
    /// 型契約変更として種別が確定した場合の内訳 (`kind` / `breaks`)。hook 出力は専用 DTO の
    /// ため、`ApiSymbolChange` に足しただけでは載らない。既存キーは変えず値を足すだけなので
    /// 既存 consumer の JSON パースは壊れない。分類できない変更では省略される。
    #[serde(rename = "contract", skip_serializing_if = "Option::is_none")]
    contract_change: Option<crate::models::review::ApiContractChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unverified: Option<&'a crate::models::review::ApiModificationUnverified>,
}

/// dead シンボル用 DTO。`HookNameFile` と分けるのは、宣言行を持つのが
/// dead / test_only だけで、api 系シンボル (add/rm/mod) は行を持たないため。
#[derive(Serialize)]
struct HookDeadSymbol<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    reference_evidence: Option<&'a crate::models::reference_evidence::ReferenceEvidence>,
    n: &'a str,
    f: &'a str,
    /// 宣言行 (0-indexed)。解決できなかった場合のみ省略。
    #[serde(rename = "l", skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
}

#[derive(Serialize)]
struct HookCompatibleModification<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    change_evidence: Option<&'a crate::models::change_evidence::ChangeEvidence>,
    n: &'a str,
    /// Rust の項目が inline module の中にあれば、ファイルの中のその経路 (Issue #282)。
    #[serde(rename = "m", skip_serializing_if = "Option::is_none")]
    inline_module: Option<&'a str>,
    f: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct HookCallbackBodyChange<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    change_evidence: Option<&'a crate::models::change_evidence::ChangeEvidence>,
    n: &'a str,
    f: &'a str,
    reason: crate::models::review::CallbackBodyChangeReason,
}

#[derive(Serialize)]
struct HookTypeAnnotationChange<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    change_evidence: Option<&'a crate::models::change_evidence::ChangeEvidence>,
    n: &'a str,
    f: &'a str,
    reason: crate::models::review::TypeAnnotationChangeReason,
    compatibility: crate::models::review::TypeAnnotationCompatibility,
}

#[derive(Serialize)]
struct HookMovedSymbol<'a> {
    n: &'a str,
    from: &'a str,
    /// Rust の項目が移動元で inline module の中にあれば、ファイルの中のその経路 (Issue #282)。
    #[serde(rename = "fm", skip_serializing_if = "Option::is_none")]
    from_inline_module: Option<&'a str>,
    to: &'a str,
    /// Rust の項目が移動先で inline module の中にあれば、ファイルの中のその経路。
    #[serde(rename = "tm", skip_serializing_if = "Option::is_none")]
    to_inline_module: Option<&'a str>,
}

/// missing_cochange の hook 用 DTO。
///
/// `c` (confidence %) だけでは 1/1 と 5/5 が同じ 100 になり、標本の大きさが読めない。
/// トリアージが「100% 共変更」の表示だけで判断して空振りするため、分子 `n` と分母 `d` を
/// 併記する (追加フィールドなので既存 consumer の JSON パースは壊れない)。
#[derive(Serialize)]
struct HookCochange<'a> {
    f: &'a str,
    w: &'a str,
    c: u32,
    /// 両方のファイルが変更されたコミット数 (confidence の分子)。
    n: usize,
    /// 集計対象となったコミット数 (confidence の分母)。算出できない場合は省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    d: Option<usize>,
    /// 証拠の作り方を blame / history の双方で明示する。
    /// 同じ `c` / `n` / `d` でも、変更行 blame (今回触った行の履歴) と
    /// ファイル履歴 (今回の変更内容と無関係な共変更も含む) では証拠の強さが違うため、
    /// トリアージがそこを読めるようにする。
    #[serde(skip_serializing_if = "Option::is_none")]
    e: Option<&'static str>,
    i: crate::models::cochange::CoChangeInterpretation,
    #[serde(skip_serializing_if = "crate::models::cochange::is_absent_or_non_finite_ranking")]
    s: Option<f64>,
}

/// 打ち切り (解析対象から外したもの) の hook 用 DTO。
/// 未追跡の巨大ファイルを除外した場合など「レビュー範囲が欠けた」ことを hook でも伝える。
/// 沈黙させると「全部見た」と読める。止めるかは理由ごとに決める
/// ([`HookBlockingCategory::for_truncation`])。`r` はモデルの安定キー (`TruncationReason` の serde 名)。
#[derive(Serialize)]
struct HookTruncation<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    f: Option<&'a str>,
    r: TruncationReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    d: Option<HookUnanalyzableSourceSummary<'a>>,
}

#[derive(Serialize)]
struct HookUnanalyzableSourceSummary<'a> {
    x: &'a str,
    n: usize,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    e: &'a [String],
}

#[derive(Serialize)]
struct HookUncertainRemoval<'a> {
    n: &'a str,
    f: &'a str,
    l: usize,
    r: TruncationReason,
}

#[derive(Serialize)]
struct HookUnresolvedRemoval<'a> {
    n: &'a str,
    f: &'a str,
    r: crate::models::review::UnresolvedRemovalReason,
    confidence: crate::models::review::UnresolvedRemovalConfidence,
    refs: Vec<HookDemandReference<'a>>,
    field_owners: Vec<HookFieldOwner<'a>>,
}

#[derive(Serialize)]
struct HookDemandReference<'a> {
    f: &'a str,
    l: usize,
    c: usize,
    d: crate::models::review::MemberTypeDemand,
}

#[derive(Serialize)]
struct HookFieldOwner<'a> {
    f: &'a str,
    n: &'a str,
    l: usize,
    c: usize,
}

#[derive(Serialize)]
struct HookApi<'a> {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    import_rebindings: Vec<&'a crate::models::python_import_rebinding::PythonImportRebinding>,
    #[serde(rename = "rm_unresolved", skip_serializing_if = "Vec::is_empty")]
    removed_unresolved: Vec<HookUnresolvedRemoval<'a>>,
    #[serde(rename = "rm_unverified", skip_serializing_if = "Vec::is_empty")]
    uncertain_removals: Vec<HookUncertainRemoval<'a>>,
    #[serde(rename = "add", skip_serializing_if = "Vec::is_empty")]
    added: Vec<HookAddedSymbol<'a>>,
    /// `add` の抽出条件の自己記述。`add` には「新規 export のうち同一 diff の**他ファイル**
    /// から実利用参照されていないもの」だけが載る (diff 内で既に他ファイルから使われる
    /// 新規 export は載らない)。この条件が出力から読めないと、載らない新規 export を
    /// 検出漏れと誤認してトリアージが走る (Issue 2026-07-27-api-add-scope-not-visible)。
    ///
    /// 旧値は `unreferenced_in_diff` だったが、同一ファイル内の参照数 `ri` が非ゼロでも
    /// `unreferenced_` を名乗るため「astro-sight が参照を数え損ねた」のか「定義ファイル外
    /// からの参照が無いという意味」なのか区別できず、トリアージが 2 度空振りした
    /// (2026-08-10 / 2026-08-11 に再発報告)。キー名は既存 consumer の JSON パースを
    /// 壊さないよう維持し、値だけを実態に合わせた。
    #[serde(rename = "add_scope", skip_serializing_if = "Option::is_none")]
    added_scope: Option<&'static str>,
    #[serde(rename = "rm", skip_serializing_if = "Vec::is_empty")]
    removed: Vec<HookNameFile<'a>>,
    #[serde(rename = "mod", skip_serializing_if = "Vec::is_empty")]
    modified: Vec<HookModifiedSymbol<'a>>,
    #[serde(rename = "mod_closed", skip_serializing_if = "Vec::is_empty")]
    modified_closed_in_diff: Vec<HookNameFile<'a>>,
    #[serde(rename = "const_value", skip_serializing_if = "Vec::is_empty")]
    const_value_changes: Vec<HookNameFile<'a>>,
    #[serde(rename = "type_annotation", skip_serializing_if = "Vec::is_empty")]
    type_annotation_changes: Vec<HookTypeAnnotationChange<'a>>,
    #[serde(rename = "callback_body", skip_serializing_if = "Vec::is_empty")]
    callback_body_changes: Vec<HookCallbackBodyChange<'a>>,
    #[serde(rename = "mod_compat", skip_serializing_if = "Vec::is_empty")]
    compatible_modified: Vec<HookCompatibleModification<'a>>,
    #[serde(skip_serializing_if = "<[crate::models::review::TypeChangeEvidence]>::is_empty")]
    type_change_evidence: &'a [crate::models::review::TypeChangeEvidence],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    moved: Vec<HookMovedSymbol<'a>>,
    #[serde(rename = "rm_dead", skip_serializing_if = "Vec::is_empty")]
    removed_dead: Vec<HookNameFile<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    property_to_field: Vec<HookNameFile<'a>>,
}

impl<'a> HookApi<'a> {
    fn from_api_changes(api: &'a ApiChanges) -> Self {
        let name_file =
            |name: &'a String, inline_module: Option<&'a str>, file: &'a String| HookNameFile {
                change_evidence: None,
                n: name.as_str(),
                inline_module,
                f: file.as_str(),
                cfg: None,
            };
        let added: Vec<HookAddedSymbol<'a>> = api
            .added
            .iter()
            .map(|change| HookAddedSymbol {
                n: change.name.as_str(),
                inline_module: change.inline_module.as_deref(),
                f: change.file.as_str(),
                refs_internal: change.refs_internal,
                cfg: change.cfg_availability.as_ref(),
            })
            .collect();
        let added_scope = (!added.is_empty()).then_some("no_cross_file_refs_in_diff");
        Self {
            added,
            import_rebindings: api.import_rebindings.iter().collect(),
            removed_unresolved: api
                .removed_unresolved
                .iter()
                .map(|removal| HookUnresolvedRemoval {
                    n: &removal.symbol.name,
                    f: &removal.symbol.file,
                    r: removal.reason,
                    confidence: removal.confidence,
                    refs: removal
                        .references
                        .iter()
                        .map(|reference| HookDemandReference {
                            f: &reference.file,
                            l: reference.line,
                            c: reference.column,
                            d: reference.demand,
                        })
                        .collect(),
                    field_owners: removal
                        .field_owners
                        .iter()
                        .map(|owner| HookFieldOwner {
                            f: &owner.file,
                            n: &owner.class,
                            l: owner.line,
                            c: owner.column,
                        })
                        .collect(),
                })
                .collect(),
            uncertain_removals: api
                .uncertain_removals
                .iter()
                .map(|s| HookUncertainRemoval {
                    n: &s.name,
                    f: &s.file,
                    l: s.line,
                    r: s.reason,
                })
                .collect(),
            added_scope,
            removed: api
                .removed
                .iter()
                .map(|change| HookNameFile {
                    cfg: change.cfg_availability.as_ref(),
                    ..name_file(&change.name, change.inline_module.as_deref(), &change.file)
                })
                .collect(),
            modified: api
                .modified
                .iter()
                .map(|change| HookModifiedSymbol {
                    change_evidence: change.change_evidence.as_ref(),
                    reference_evidence: change.reference_evidence.as_ref(),
                    n: change.name.as_str(),
                    inline_module: change.inline_module.as_deref(),
                    f: change.file.as_str(),
                    no_resolved_internal_callers: change.no_resolved_internal_callers,
                    contract_change: change.contract_change,
                    unverified: change.unverified.as_ref(),
                })
                .collect(),
            modified_closed_in_diff: api
                .modified_closed_in_diff
                .iter()
                .map(|change| HookNameFile {
                    change_evidence: change.change_evidence.as_ref(),
                    ..name_file(&change.name, change.inline_module.as_deref(), &change.file)
                })
                .collect(),
            const_value_changes: api
                .const_value_changes
                .iter()
                .map(|change| HookNameFile {
                    change_evidence: change.change_evidence.as_ref(),
                    ..name_file(&change.name, change.inline_module.as_deref(), &change.file)
                })
                .collect(),
            type_annotation_changes: api
                .type_annotation_changes
                .iter()
                .map(|change| HookTypeAnnotationChange {
                    change_evidence: change.change.change_evidence.as_ref(),
                    n: change.change.name.as_str(),
                    f: change.change.file.as_str(),
                    reason: change.reason,
                    compatibility: change.compatibility,
                })
                .collect(),
            compatible_modified: api
                .compatible_modified
                .iter()
                .map(|change| HookCompatibleModification {
                    change_evidence: change.change_evidence.as_ref(),
                    n: change.name.as_str(),
                    inline_module: change.inline_module.as_deref(),
                    f: change.file.as_str(),
                    reason: change.reason.as_str(),
                })
                .collect(),
            type_change_evidence: &api.type_change_evidence,
            callback_body_changes: api
                .callback_body_changes
                .iter()
                .map(|entry| HookCallbackBodyChange {
                    change_evidence: entry.change.change_evidence.as_ref(),
                    n: &entry.change.name,
                    f: &entry.change.file,
                    reason: entry.reason,
                })
                .collect(),
            moved: api
                .moved
                .iter()
                .map(|change| HookMovedSymbol {
                    n: change.name.as_str(),
                    from: change.from.as_str(),
                    from_inline_module: change.from_inline_module.as_deref(),
                    to: change.to.as_str(),
                    to_inline_module: change.to_inline_module.as_deref(),
                })
                .collect(),
            removed_dead: api
                .removed_dead
                .iter()
                .map(|change| HookNameFile {
                    cfg: change.cfg_availability.as_ref(),
                    ..name_file(&change.name, change.inline_module.as_deref(), &change.file)
                })
                .collect(),
            property_to_field: api
                .property_to_field
                .iter()
                .map(|change| name_file(&change.name, None, &change.file))
                .collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.import_rebindings.is_empty()
            && self.added.is_empty()
            && self.removed_unresolved.is_empty()
            && self.uncertain_removals.is_empty()
            && self.removed.is_empty()
            && self.modified.is_empty()
            && self.modified_closed_in_diff.is_empty()
            && self.const_value_changes.is_empty()
            && self.type_annotation_changes.is_empty()
            && self.callback_body_changes.is_empty()
            && self.compatible_modified.is_empty()
            && self.type_change_evidence.is_empty()
            && self.moved.is_empty()
            && self.removed_dead.is_empty()
            && self.property_to_field.is_empty()
    }
}

fn impact_groups(groups: &BTreeMap<String, HookImpactGroup>) -> Vec<HookImpact<'_>> {
    groups
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
        .collect()
}

/// diff 外の caller を `HookImpactGroup` に集約する共通ループ。
/// impacted / informational の 2 経路は「caller を走査し、diff 外の caller の symbols を
/// 述語で絞って `BTreeMap<changed_path, group>` に積む」構造が同一で、残す symbol の条件
/// (`keep_symbol`) だけが異なるため、述語を渡して共通化する。
fn accumulate_hook_impacts(
    target: &mut BTreeMap<String, HookImpactGroup>,
    change_path: &str,
    callers: &[ImpactedCaller],
    resolution: &DiffCallerResolution,
    dir: &str,
    keep_symbol: impl Fn(&str) -> bool,
) {
    for caller in callers {
        if resolution.is_resolved(dir, &caller.path, caller.line) {
            continue;
        }
        let causal_syms: Vec<String> = caller
            .symbols
            .iter()
            .filter(|sym| keep_symbol(sym.as_str()))
            .cloned()
            .collect();
        if causal_syms.is_empty() {
            continue;
        }
        let entry = target.entry(change_path.to_string()).or_default();
        for sym in &causal_syms {
            entry.changed_symbols.insert(sym.clone());
        }
        entry
            .refs
            .push((caller.path.clone(), caller.line, causal_syms));
    }
}

/// api の判定に合わせて、Stop hook の impact の参照を振り分けるための `(ファイル, 名前)` の集合。
///
/// api 側で「互換 / 追随済み / 値のみ変更」と判定済みの modified 系シンボルは、Stop hook の impact
/// でも informational として扱う。ここを揃えないと api.mod_compat 自体は非 blocking なのに、同じ
/// シンボルの参照一覧が impacts として blocking になる。
struct HookImpactPolicy<'a> {
    /// 参照を hook の停止に使わない modified 系シンボル。
    informational_modified: HashSet<(&'a str, &'a str)>,
    /// 型注釈だけの変更。互換性は未確認なので、strict でも参照の分類は保ち、専用 policy で確認を求める。
    annotation_info: HashSet<(&'a str, &'a str)>,
    /// 同じ値を指すと証明した Python の import の付け替え。
    import_rebinding_info: HashSet<(&'a str, &'a str)>,
    /// 省略可能な引数・props の削除。リポジトリ内の利用の証拠に基づく分類なので、証拠にした
    /// 呼び出し箇所を確認箇所として残す。
    optional_removal_info: HashSet<(&'a str, &'a str)>,
}

impl<'a> HookImpactPolicy<'a> {
    fn new(api: &'a ApiChanges, strict_const_values: bool) -> Self {
        let annotation_info: HashSet<(&str, &str)> = api
            .type_annotation_changes
            .iter()
            .map(|m| (m.change.file.as_str(), m.change.name.as_str()))
            .chain(
                api.unchanged_type_annotations
                    .iter()
                    .map(|m| (m.file.as_str(), m.name.as_str())),
            )
            .collect();
        let mut informational_modified: HashSet<(&str, &str)> = api
            .compatible_modified
            .iter()
            // `const` の追加は qualname ごとの証明なので、同じ素の名前の別の宣言 (別の impl の
            // メソッド・cfg の分岐) の呼び出し元までまとめて外さない。呼び出し元は impact
            // エンジンが同名の宣言まで確かめて情報提供に分ける。
            .filter(|m| m.reason != "rust_const_fn_added")
            .map(|m| (m.file.as_str(), m.name.as_str()))
            .chain(
                api.modified_closed_in_diff
                    .iter()
                    .map(|m| (m.file.as_str(), m.name.as_str())),
            )
            .chain(
                api.unchanged_static_consts
                    .iter()
                    .map(|m| (m.file.as_str(), m.name.as_str())),
            )
            .chain(annotation_info.iter().copied())
            .collect();
        if !strict_const_values {
            informational_modified.extend(
                api.callback_body_changes
                    .iter()
                    .map(|m| (m.change.file.as_str(), m.change.name.as_str())),
            );
            informational_modified.extend(
                api.const_value_changes
                    .iter()
                    .map(|m| (m.file.as_str(), m.name.as_str())),
            );
        }
        let import_rebinding_info = api
            .import_rebindings
            .iter()
            .filter(|entry| {
                entry.equivalence
                    == crate::models::python_import_rebinding::RebindingEquivalence::Proven
            })
            .map(|entry| (entry.file.as_str(), entry.name.as_str()))
            .collect();
        let optional_removal_info = api
            .compatible_modified
            .iter()
            .filter(|m| {
                matches!(
                    m.reason.as_str(),
                    "trailing_optional_params_removed" | "optional_object_props_removed"
                )
            })
            .map(|m| (m.file.as_str(), m.name.as_str()))
            .collect();
        Self {
            informational_modified,
            annotation_info,
            import_rebinding_info,
            optional_removal_info,
        }
    }

    /// `impacted_callers` の参照のうち、hook を止める原因に残すシンボルか。breaking な変更
    /// (modified / removed) だけを残し、`added` 由来と informational 扱いの modified api シンボルは外す。
    fn blocks(&self, path: &str, flags: Option<&HookChangeFlags>, symbol: &str) -> bool {
        flags.is_some_and(|flags| flags.has_non_added)
            && !self.informational_modified.contains(&(path, symbol))
    }

    /// `impacted_callers` の参照のうち、確認箇所として `impact_info` に残すシンボルか
    /// (型注釈の変更・省略可能な引数や props の削除・証明済みの import の付け替え)。
    fn informs(&self, path: &str, flags: Option<&HookChangeFlags>, symbol: &str) -> bool {
        (flags.is_some_and(|flags| flags.has_modified)
            && (self.annotation_info.contains(&(path, symbol))
                || self.optional_removal_info.contains(&(path, symbol))))
            || self.import_rebinding_info.contains(&(path, symbol))
    }
}

/// hook を止める参照 (`impacts`) と、確認箇所として残す参照 (`impact_info`) のグループ。
/// キーは変更したファイル。
#[derive(Default)]
struct HookImpactGroups {
    unresolved: BTreeMap<String, HookImpactGroup>,
    informational: BTreeMap<String, HookImpactGroup>,
}

/// 影響分析の呼び出し側を、diff 内で解決済みのものを除いて hook の 2 つのグループに振り分ける。
fn collect_hook_impact_groups(
    result: &ReviewResult,
    policy: &HookImpactPolicy<'_>,
    resolution: &DiffCallerResolution,
    dir: &str,
) -> HookImpactGroups {
    let mut groups = HookImpactGroups::default();
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
        // caller 側は名前だけを持つ。同名の別 owner に added と modified が混在しても、
        // 後から現れた added で breaking な変更を上書きしない。
        let mut affected_change_types: HashMap<&str, HookChangeFlags> = HashMap::new();
        for symbol in &change.affected_symbols {
            let flags = affected_change_types
                .entry(symbol.name.as_str())
                .or_default();
            flags.has_non_added |= symbol.change_type != "added";
            flags.has_modified |= symbol.change_type == "modified";
        }
        let path = change.path.as_str();
        let flags = |symbol: &str| affected_change_types.get(symbol);
        accumulate_hook_impacts(
            &mut groups.unresolved,
            path,
            &change.impacted_callers,
            resolution,
            dir,
            |symbol| policy.blocks(path, flags(symbol), symbol),
        );
        accumulate_hook_impacts(
            &mut groups.informational,
            path,
            &change.impacted_callers,
            resolution,
            dir,
            |symbol| policy.informs(path, flags(symbol), symbol),
        );
        // informational: modified 由来のみを低信号として残す。
        accumulate_hook_impacts(
            &mut groups.informational,
            path,
            &change.informational_callers,
            resolution,
            dir,
            |symbol| flags(symbol).is_some_and(|flags| flags.has_modified),
        );
    }

    // API の呼び出し契約で未証明だった参照は、ファイルや行の変更だけでは解決済みにしない。
    for call in &result.api_changes.unresolved_modified_callers {
        if !result
            .api_changes
            .modified
            .iter()
            .any(|change| change.file == call.file && change.name == call.name)
        {
            continue;
        }
        let group = groups.unresolved.entry(call.file.clone()).or_default();
        group.changed_symbols.insert(call.name.clone());
        if let Some((_, _, symbols)) = group
            .refs
            .iter_mut()
            .find(|(path, line, _)| *path == call.reference.path && *line == call.reference.line)
        {
            symbols.push(call.name.clone());
            symbols.sort_unstable();
            symbols.dedup();
        } else {
            group.refs.push((
                call.reference.path.clone(),
                call.reference.line,
                vec![call.name.clone()],
            ));
        }
        group.refs.sort_unstable();
    }
    groups
}

/// hook の JSON の組み立て。項目を載せること・hook の出力を出すこと・hook を止めることは一致しない
/// (未確定の構文の証拠は、ほかの通知に添えるだけで、それ単独では出力を出さない) ので分けて持つ。
#[derive(Default)]
struct HookReport {
    fields: serde_json::Map<String, serde_json::Value>,
    blocking_categories: Vec<HookBlockingCategory>,
    notifies: bool,
}

impl HookReport {
    /// 項目を載せる。それ単独では hook の出力を出さない。
    fn attach(&mut self, key: &str, value: impl Serialize) {
        self.fields.insert(
            key.to_owned(),
            serde_json::to_value(value).expect("hook DTO should serialize"),
        );
    }

    /// 項目を載せ、hook の出力を出す。
    fn notify(&mut self, key: &str, value: impl Serialize) {
        self.attach(key, value);
        self.notifies = true;
    }

    /// hook を止める理由を足す (出力の順と重複の除去は `finish` で行う)。
    fn block(&mut self, category: HookBlockingCategory) {
        self.blocking_categories.push(category);
    }

    fn finish(self) -> HookJsonBuild {
        let Self {
            mut fields,
            mut blocking_categories,
            notifies,
        } = self;
        if !notifies {
            return HookJsonBuild {
                value: None,
                is_blocking: false,
            };
        }
        // 空配列も「情報提供だけで停止しない」という事実なので省略しない。
        blocking_categories.sort_unstable();
        blocking_categories.dedup();
        let is_blocking = !blocking_categories.is_empty();
        fields.insert(
            "blocking_categories".into(),
            serde_json::to_value(blocking_categories).expect("hook categories should serialize"),
        );
        fields.insert(
            "hint".into(),
            serde_json::Value::String("False positives? Run astro-sight-triage skill.".into()),
        );
        // TOON 用の preserve_order に依らず、従来の JSON キー順を保つ。
        let mut value = serde_json::Value::Object(fields);
        value.sort_all_objects();
        HookJsonBuild {
            value: Some(value),
            is_blocking,
        }
    }
}

/// diff を持たない呼び出し (単体テスト) 用。「diff 内で解決済み」の判定は影響分析の結果に
/// 現れたファイルだけで行う。
#[cfg(test)]
pub(crate) fn build_review_hook_json(
    result: &ReviewResult,
    dir: &str,
    strict_const_values: bool,
) -> HookJsonBuild {
    build_review_hook_json_for_diff(result, dir, strict_const_values, false, "", &[])
}

/// `diff_input` / `diff_files` はレビュー対象の diff。呼び出し側が diff 内で解決済みか
/// (`DiffCallerResolution`) の判定に使う。
pub(crate) fn build_review_hook_json_for_diff(
    result: &ReviewResult,
    dir: &str,
    strict_const_values: bool,
    strict_type_annotations: bool,
    diff_input: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> HookJsonBuild {
    let policy = HookImpactPolicy::new(&result.api_changes, strict_const_values);
    // 呼び出し側が diff 内で解決済みかは `impact --hook` と同じ判定 (`DiffCallerResolution`) にする。
    // 影響分析の結果 (`impact.changes`) だけで判定すると、トップレベルの呼び出しだけを更新した
    // ファイルが diff 外扱いになり誤ってブロックする。
    let resolution = DiffCallerResolution::build(
        dir,
        result.impact.changes.iter().map(|c| c.path.as_str()),
        diff_input,
        diff_files,
    );
    let groups = collect_hook_impact_groups(result, &policy, &resolution, dir);

    let mut report = HookReport::default();
    add_change_evidence(&mut report, result);
    // impacts: [{src,syms,refs:[{p,ln,s}]}]
    if !groups.unresolved.is_empty() {
        report.block(HookBlockingCategory::Impacts);
        report.notify("impacts", impact_groups(&groups.unresolved));
    }
    // impact_info: import-only など blocking しない低信号 impact。`impacts` と分ける。
    if !groups.informational.is_empty() {
        report.notify("impact_info", impact_groups(&groups.informational));
    }
    add_cochanges(&mut report, result);
    if !result.rename_candidates.is_empty() {
        report.notify("rename_candidates", &result.rename_candidates);
    }
    if !result.rename_candidate_diagnostics.is_empty() {
        report.notify(
            "rename_candidate_diagnostics",
            &result.rename_candidate_diagnostics,
        );
    }
    add_api(
        &mut report,
        result,
        strict_const_values,
        strict_type_annotations,
    );
    if !result.api_changes.added_reference_summary.is_empty() {
        report.notify(
            "added_reference_summary",
            &result.api_changes.added_reference_summary,
        );
    }
    add_truncations(&mut report, result);
    add_dead_code(&mut report, result);
    report.finish()
}

/// 呼び出し側を持たない本体変更も証拠を残す。停止カテゴリは従来の判定だけで決める。
fn add_change_evidence(report: &mut HookReport, result: &ReviewResult) {
    let syntax_changes: Vec<_> = result
        .impact
        .changes
        .iter()
        .flat_map(|change| {
            change.affected_symbols.iter().filter_map(|symbol| {
                Some(HookChangeEvidence {
                    f: &change.path,
                    n: &symbol.name,
                    k: &symbol.kind,
                    change_evidence: symbol.change_evidence.as_ref()?,
                })
            })
        })
        .collect();
    if syntax_changes.is_empty() {
        return;
    }
    // 未確定・トークン不変だけでは通知を増やさず、既存の通知には証拠を残す。
    let proven = syntax_changes.iter().any(|change| {
        change.change_evidence.analysis == crate::models::change_evidence::ChangeAnalysis::Known
            && !change.change_evidence.changed_parts.is_empty()
    });
    if proven {
        report.notify("change_evidence", syntax_changes);
    } else {
        report.attach("change_evidence", syntax_changes);
    }
}

/// cochange: [{f,w,c}] — 情報提供のみ。hook は止めない。
fn add_cochanges(report: &mut HookReport, result: &ReviewResult) {
    if result.missing_cochanges.is_empty() {
        return;
    }
    let cochanges: Vec<HookCochange<'_>> = result
        .missing_cochanges
        .iter()
        .map(|cochange| HookCochange {
            f: cochange.file.as_str(),
            w: cochange.expected_with.as_str(),
            c: (cochange.confidence * 100.0).round() as u32,
            n: cochange.co_changes,
            d: cochange.denominator,
            e: match cochange.evidence {
                Some(crate::models::cochange::CoChangeEvidence::History) => Some("history"),
                Some(crate::models::cochange::CoChangeEvidence::Blame) | None => Some("blame"),
            },
            i: cochange.interpretation.clone(),
            s: cochange.ranking_score,
        })
        .collect();
    report.notify("cochange", cochanges);
}

/// api: {add,rm,mod,moved,property_to_field,rm_dead,const_value} — 空でないセクションのみ。
///
/// api.added / api.moved / api.property_to_field / api.removed_dead / api.const_value は
/// 破壊的変更ではないため Stop hook のブロッキング対象から外し informational 扱いにする。
/// api.removed / api.modified は破壊的変更の可能性があるため従来どおり blocking。
/// const_value (値のみ変更) は `--strict-public-const-values` 指定時のみ blocking に昇格する。
fn add_api(
    report: &mut HookReport,
    result: &ReviewResult,
    strict_const_values: bool,
    strict_type_annotations: bool,
) {
    let api = HookApi::from_api_changes(&result.api_changes);
    if api.is_empty() {
        return;
    }
    // test-only の根拠は同じレビューの宣言に限る。解析範囲が欠けた結果や、宣言位置が
    // 分からない候補は停止判定の降格に使わない。表示する API の分類・項目は維持する。
    let test_only_api_symbols: HashSet<(&str, &str, &str)> = if result.truncations.is_empty()
        && result.impact.truncations.is_empty()
        && result.skipped.is_none()
    {
        result
            .test_only_symbols
            .iter()
            .filter(|symbol| symbol.line.is_some())
            .map(|symbol| {
                (
                    symbol.file.as_str(),
                    symbol.name.as_str(),
                    symbol.kind.as_str(),
                )
            })
            .collect()
    } else {
        HashSet::new()
    };
    let is_test_only = |change: &crate::models::review::ApiSymbolChange| {
        test_only_api_symbols.contains(&(
            change.file.as_str(),
            change.name.as_str(),
            change.kind.as_str(),
        ))
    };
    if !api.removed.is_empty() {
        report.block(HookBlockingCategory::ApiRm);
    }
    if result
        .api_changes
        .modified
        .iter()
        .any(|change| !is_test_only(change))
    {
        report.block(HookBlockingCategory::ApiMod);
    }
    if strict_const_values
        && result
            .api_changes
            .const_value_changes
            .iter()
            .any(|change| !is_test_only(change))
    {
        report.block(HookBlockingCategory::ApiConstValue);
    }
    if strict_const_values && !api.callback_body_changes.is_empty() {
        report.block(HookBlockingCategory::ApiCallbackBody);
    }
    if strict_type_annotations && !api.type_annotation_changes.is_empty() {
        report.block(HookBlockingCategory::PolicyTypeAnnotation);
    }
    report.notify("api", api);
}

/// trunc: [{f,r,d?}] — 解析対象から外したものの申告。API の判定を確かめられない理由だけ hook を止める。
fn add_truncations(report: &mut HookReport, result: &ReviewResult) {
    if result.truncations.is_empty() {
        return;
    }
    for truncation in &result.truncations {
        if let Some(category) = HookBlockingCategory::for_truncation(truncation.reason) {
            report.block(category);
        }
    }
    let truncations: Vec<HookTruncation<'_>> = result
        .truncations
        .iter()
        .map(|t| HookTruncation {
            f: t.path.as_deref(),
            r: t.reason,
            d: if t.reason == TruncationReason::UnanalyzableSource {
                t.detail
                    .as_ref()
                    .map(|detail| HookUnanalyzableSourceSummary {
                        x: &detail.extension,
                        n: detail.count,
                        e: &detail.examples,
                    })
            } else {
                None
            },
        })
        .collect();
    report.notify("trunc", truncations);
}

/// dead: [{n,f,l}] と、参照の証拠を伴う test-only・外部入口・未確認のシンボル。
fn add_dead_code(report: &mut HookReport, result: &ReviewResult) {
    if !result.dead_symbols.is_empty() {
        report.block(HookBlockingCategory::Dead);
        report.notify("dead", hook_dead_symbols(&result.dead_symbols));
    }
    if result
        .test_only_symbols
        .iter()
        .any(|symbol| symbol.reference_evidence.is_some())
    {
        report.notify("test_only", hook_dead_symbols(&result.test_only_symbols));
    }
    for (key, symbols) in [
        ("external_entry", &result.external_entry_symbols),
        ("unverified", &result.unverified_symbols),
    ] {
        if !symbols.is_empty() {
            report.notify(key, hook_dead_symbols(symbols));
        }
    }
}

fn hook_dead_symbols(symbols: &[crate::models::review::DeadSymbol]) -> Vec<HookDeadSymbol<'_>> {
    symbols
        .iter()
        .map(|symbol| HookDeadSymbol {
            reference_evidence: symbol.reference_evidence.as_ref(),
            n: &symbol.name,
            f: &symbol.file,
            line: symbol.line,
        })
        .collect()
}

/// --hook 時の review 出力: compact JSON を stderr に出力する。
///
/// ここは Stop hook の JSON 契約なので、`OutputOptions` を受け取らず
/// `serde_json::Value` の `Display` (= compact JSON) で直接書く。
/// `serialize_document` を通すと `--format auto` で TOON が選ばれ、黙って契約が壊れる
/// (`OutputOptions::ensure_json_protocol` の不変条件を参照)。
/// 戻り値は blocking な検出 (impacts / api / dead) があったか。呼び出し側はこれを見て exit 1 にし、
/// cochange のみの informational な出力は exit 0 にして Stop hook を止めない。
///
/// ここで `process::exit` しないのは、exit がデストラクタを走らせないため。staged review の
/// 一時スナップショット (`TempDir`) が消えずに残り、Stop hook が止めるたびに増えていた。
pub(crate) fn review_hook_output(
    result: &ReviewResult,
    dir: &str,
    strict_const_values: bool,
    strict_type_annotations: bool,
    diff_input: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> Result<bool> {
    let build = build_review_hook_json_for_diff(
        result,
        dir,
        strict_const_values,
        strict_type_annotations,
        diff_input,
        diff_files,
    );
    let Some(hook_output) = build.value else {
        return Ok(false);
    };

    eprintln!("{hook_output}");
    Ok(build.is_blocking)
}
