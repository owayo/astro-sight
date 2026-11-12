//! 追加 / 削除 / 変更されたファイルごとの API 差分分類。
//!
//! 入口は変更ファイルの [`process_modified_file`] と、`files` (追加・削除の候補と、追加・削除した
//! ファイル)・`moves` (移動と再公開)・`moved_availability` / `availability` (有る設定の比較) の関数。
//! シグネチャの変わった宣言は `signature` (ファイルの組の文脈と順序付きの規則)・`contract` (互換と
//! 呼び出し側の追随)・`record` (bucket への記録) で判定する。Rust の変更ファイルは `rust_modified`、
//! signature に現れない Python の契約は `python_contracts` が扱う。

use super::*;
use crate::models::review::{ApiModificationUnverified, ApiModificationUnverifiedReason};

mod availability;
mod contract;
mod files;
mod moved_availability;
mod moves;
mod python_contracts;
mod record;
mod rust_modified;
mod signature;

pub(crate) use availability::*;
use contract::*;
pub(crate) use files::*;
pub(crate) use moved_availability::compare_moved_availability;
pub(crate) use moves::*;
use python_contracts::*;
use record::*;
use rust_modified::*;
use signature::*;

/// 通常の modified ファイル (old_path / new_path がどちらも `/dev/null` でない) で、
/// added (新規シンボル) / removed (削除シンボル) / modified (シグネチャ変更) を分類する。
///
/// 全ての pub/exported シンボルの組み合わせを評価し、Rust private module 抑制 /
/// bin-only crate 抑制 / 内部参照 / 同一 diff 内 closed-in-diff / TS optional destructure 等
/// の各種抑制ルールを適用する。最も複雑な処理パス。
///
/// `old_syms` / `new_syms` / `in_file_callees` は `detect_api_changes` の Phase 0 で
/// 抽出済みのものを受け取る (rename 差分では base 側に新パスが存在しないため、旧版は
/// old_path 由来)。cross-file 参照判定は事前構築済みの `ref_index` を参照する。
pub(crate) fn process_modified_file(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
) {
    let maps = SymbolMaps::build(facts);
    // 追加 → 削除 → 変更 の順で振り分ける。削除判定は追加判定が集めた
    // 「同ファイル内の新規シンボル名」を rename 判定に使うため、この順序に依存する。
    let new_symbols_in_current_file = collect_added_symbols(inputs, state, df, facts, &maps);
    collect_removed_symbols(
        inputs,
        state,
        df,
        facts,
        &maps,
        &new_symbols_in_current_file,
    );
    collect_modified_symbols(inputs, state, df, facts, &maps);
    if let (Some(pairing), Some(declarations)) = (&maps.rust, facts.rust_declarations) {
        collect_rust_availability_changes(inputs, state, df, facts, pairing, declarations);
    }
    // シグネチャ差分に乗らない契約変更。上の 3 つと違い「old/new でシグネチャが同じ」
    // シンボルを見るため、最後に独立した経路として走らせる。
    collect_python_contract_changes(inputs, state, df, facts, &maps);
}
