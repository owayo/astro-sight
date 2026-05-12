//! Pass 2 (streaming fold) で使う per-worker 状態と visitor 実装。
//!
//! - `WorkerState`: rayon fold の local state。chunk 内で再利用され、chunk 終了時に
//!   merge → drop される。
//! - `RefEventMini`: 1 件の reference event を最小サイズで保持する内部 struct。
//!   `on_ref` 時点で import 判定と caller_name intern まで済ませておき、context の
//!   文字列コピーを廃して per-file バッファの heap を削減する。
//! - `ImpactCollector`: `refs::RefVisitor` を実装し、per-file visit の callback を
//!   直接受け取って `finish_file` で Stage 1-6 (Stage 4b 除く) を適用する。
use camino::Utf8Path;
use lru::LruCache;

use crate::engine::refs;
use crate::language::LangId;

use super::signature::extract_function_from_context;
use super::test_context::is_ref_in_target_test_context;
use super::types::{StringPool, SymEntries, TypedCallerMap};
use super::{FileContext, ParsedFile, ci_key, filters, lang_compat_group};

/// per-worker の中間状態。per-file バッファ (`ref_hit` / `ref_events` / `def_events`) は
/// `finish_file` / `reset_buffers` で再利用されるため、巨大ファイルでも再割当ては発生しない。
pub(super) struct WorkerState {
    pub(super) local_maps: Vec<TypedCallerMap>,
    /// Phase 4: 低確信度 caller (BareNameOnly + generic name) の振り分け先。
    /// `local_maps` と同じ shape (per FileContext) で並走し、`build_file_impact` で
    /// `low_confidence_callers` に変換される。
    pub(super) local_low_maps: Vec<TypedCallerMap>,
    pub(super) local_def_paths: Vec<Vec<u32>>,
    pub(super) target_cache: LruCache<String, Option<ParsedFile>>,
    pub(super) ref_hit: Vec<bool>,
    pub(super) ref_events: Vec<RefEventMini>,
    pub(super) def_events: Vec<u32>,
}

/// 1 件の reference event を最小サイズで表現する。
///
/// `on_ref` 時点で import 判定と caller_name 抽出・intern まで済ませておき、
/// `context` 文字列自体は保持しない（per-file buffer の heap を劇的に削減）。
/// sym_ix / line / column / caller_name_id / is_import_flag / confidence の計 24 B 構造体 + 1 bit + 1 byte。
pub(super) struct RefEventMini {
    pub(super) sym_ix: u32,
    pub(super) line: u32,
    pub(super) column: u32,
    pub(super) caller_name_id: u32,
    pub(super) is_import: bool,
    /// 0 = ExactOwner, 1 = InferredOwner, 2 = BareNameOnly。
    /// Phase 4 (impact ルーティング) で `BareNameOnly` + generic name のシンボル参照を
    /// `low_confidence_callers` へ振り分けるために使う。
    pub(super) confidence: u8,
}

/// 汎用すぎてシンボル名だけでは owner を特定できない PHP/JS 系メソッド名。
/// `BareNameOnly` (`$x->new()` のような receiver 不明呼び出し) かつシンボル名側が
/// このリストに含まれる場合は impact 信号として弱いため、`low_confidence_callers`
/// に振り分けて `impacted_callers` を汚染しない。
const GENERIC_METHOD_NAMES: &[&str] = &[
    "new", "update", "save", "delete", "get", "set", "find", "create", "make", "build", "init",
    "add", "remove",
];

/// シンボル名が generic method name（owner なしでは曖昧すぎる名前）かを判定する。
///
/// PHP は case-insensitive 識別子ではないが、念のため `eq_ignore_ascii_case` で比較する
/// (キーワード集合が小さく ASCII のみのため、性能とロケール非依存性を両立)。
pub(super) fn is_generic_method_name(name: &str) -> bool {
    GENERIC_METHOD_NAMES
        .iter()
        .any(|g| g.eq_ignore_ascii_case(name))
}

/// `RefEventMini` を低確信度 caller bucket に振り分けるか判定する。
///
/// 条件: `confidence == 2` (BareNameOnly) かつ参照対象シンボル名が generic name。
/// Phase 4 ルーティングのコア判定。`ASTRO_SIGHT_NO_CONFIDENCE_FILTER=1` での無効化は
/// 呼び出し側 (`finish_file`) で行う。
pub(super) fn is_low_confidence_caller(e: &RefEventMini, all_symbol_names: &[String]) -> bool {
    if e.confidence != 2 {
        return false;
    }
    let Some(name) = all_symbol_names.get(e.sym_ix as usize) else {
        return false;
    };
    is_generic_method_name(name)
}

/// 環境変数 `ASTRO_SIGHT_NO_CONFIDENCE_FILTER=1` で低確信度ルーティングを無効化する。
/// デバッグ目的で全 caller を `impacted_callers` に流したいときのみ使用する。
fn confidence_filter_disabled() -> bool {
    std::env::var("ASTRO_SIGHT_NO_CONFIDENCE_FILTER")
        .ok()
        .as_deref()
        == Some("1")
}

/// `RefVisitor` の実装: per-file の ref 走査中は最小限の buffering だけ行い、
/// ファイル走査完了後に `finish_file` で Stage 1-6 (Stage 4b 除く) の filter を適用して
/// `local_maps` / `local_def_paths` へ流す。`SymbolReference` の Vec は生成しない。
pub(super) struct ImpactCollector<'a> {
    pub(super) sym_to_fc: &'a [Vec<u32>],
    pub(super) file_contexts: &'a [FileContext],
    pub(super) all_symbol_names: &'a [String],
    pub(super) parent_ix_by_sym: &'a [Option<usize>],
    pub(super) pool: &'a std::sync::Mutex<StringPool>,
    pub(super) path_str: &'a str,

    pub(super) local_maps: &'a mut [TypedCallerMap],
    /// Phase 4: 低確信度 caller の振り分け先 (`WorkerState::local_low_maps` の slice)。
    pub(super) local_low_maps: &'a mut [TypedCallerMap],
    pub(super) local_def_paths: &'a mut [Vec<u32>],
    pub(super) target_cache: &'a mut LruCache<String, Option<ParsedFile>>,

    pub(super) ref_hit: &'a mut [bool],
    pub(super) ref_events: &'a mut Vec<RefEventMini>,
    pub(super) def_events: &'a mut Vec<u32>,
}

impl<'a> refs::RefVisitor for ImpactCollector<'a> {
    fn on_ref(
        &mut self,
        sym_ix: u32,
        line: usize,
        column: usize,
        context: &str,
        is_def: bool,
        confidence: crate::models::reference::RefConfidence,
    ) {
        let ix = sym_ix as usize;
        if ix < self.ref_hit.len() {
            self.ref_hit[ix] = true;
        }
        if is_def {
            self.def_events.push(sym_ix);
            return;
        }

        // Stage 6 (import 行) の判定は文字列のままでないと行えないため、ここで即決する。
        // caller_name も context から抽出し、pool へ intern して ID にしてから push する。
        // これにより `RefEventMini` は固定長で済み、per-file バッファの heap を削減する。
        let is_import = filters::is_import_context(Some(context));
        let caller_name_fallback = || self.all_symbol_names.get(ix).cloned().unwrap_or_default();
        let caller_name =
            extract_function_from_context(context).unwrap_or_else(caller_name_fallback);
        let caller_name_id = self
            .pool
            .lock()
            .expect("string pool mutex poisoned")
            .intern(&caller_name);

        let confidence_u8 = match confidence {
            crate::models::reference::RefConfidence::ExactOwner => 0,
            crate::models::reference::RefConfidence::InferredOwner => 1,
            crate::models::reference::RefConfidence::BareNameOnly => 2,
        };

        self.ref_events.push(RefEventMini {
            sym_ix,
            line: line as u32,
            column: column as u32,
            caller_name_id,
            is_import,
            confidence: confidence_u8,
        });
    }
}

impl<'a> ImpactCollector<'a> {
    /// ファイル走査完了時に呼ぶ。buffered events に対して Stage 1-6 (Stage 4b 除く) の
    /// filter を適用し、`local_maps` / `local_def_paths` に push する。バッファは clear して
    /// 次ファイルで再利用する。
    pub(super) fn finish_file(self) {
        // Definition: path を 1 回だけ intern して全 def sym へ配布
        if !self.def_events.is_empty() {
            let path_id = self
                .pool
                .lock()
                .expect("string pool mutex poisoned")
                .intern(self.path_str);
            for &ix in self.def_events.iter() {
                if let Some(paths) = self.local_def_paths.get_mut(ix as usize) {
                    paths.push(path_id);
                }
            }
            self.def_events.clear();
        }

        // References: 1 件ずつ Stage 1-6 (Stage 4b 除く) の filter を適用し local_maps へ流す
        // ref_events を drain することで Vec のヒープは再利用される。Stage 6 (import 判定) と
        // caller_name の抽出は on_ref 時点で済ませてあるため、ここでは flag / ID で判定する。
        // Phase 4: confidence == BareNameOnly + シンボル名が generic (new/update/...) なら
        // local_low_maps へ振り分け、強い impact 信号を汚染しない。
        let filter_disabled = confidence_filter_disabled();
        for e in self.ref_events.drain(..) {
            if e.is_import {
                continue;
            }

            let sym_ix_usize = e.sym_ix as usize;
            let fc_ixs = &self.sym_to_fc[sym_ix_usize];
            if fc_ixs.is_empty() {
                continue;
            }

            let has_parent_type = self.parent_ix_by_sym[sym_ix_usize].is_some();
            let parent_in_this_file = self.parent_ix_by_sym[sym_ix_usize]
                .and_then(|pix| self.ref_hit.get(pix))
                .copied()
                .unwrap_or(false);
            let route_low = !filter_disabled && is_low_confidence_caller(&e, self.all_symbol_names);

            for &fc_ix_raw in fc_ixs {
                let fc_ix = fc_ix_raw as usize;
                let ctx = &self.file_contexts[fc_ix];
                let source_path = &ctx.new_path;
                let source_lang_group = lang_compat_group(ctx.lang_id);

                if filters::is_same_source_file(self.path_str, source_path) {
                    continue;
                }
                if let Ok(ref_lang) = LangId::from_path(Utf8Path::new(self.path_str))
                    && lang_compat_group(ref_lang) != source_lang_group
                {
                    continue;
                }
                if has_parent_type && !parent_in_this_file {
                    continue;
                }
                if is_ref_in_target_test_context(
                    self.path_str,
                    e.line as usize,
                    e.column as usize,
                    self.target_cache,
                ) {
                    continue;
                }

                let sym_key_canonical = &self.all_symbol_names[sym_ix_usize];
                let affected_sym_name = ctx
                    .affected
                    .iter()
                    .find(|a| ci_key(ctx.lang_id, &a.name) == *sym_key_canonical)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| sym_key_canonical.clone());

                let (path_id, sym_name_id) = {
                    let mut p = self.pool.lock().expect("string pool mutex poisoned");
                    (p.intern(self.path_str), p.intern(&affected_sym_name))
                };

                let ix_u32 = e.sym_ix;
                let key = (path_id, e.line as usize);
                let target_map = if route_low {
                    &mut self.local_low_maps[fc_ix]
                } else {
                    &mut self.local_maps[fc_ix]
                };
                let entry = target_map
                    .entry(key)
                    .or_insert_with(|| (e.caller_name_id, SymEntries::new()));
                if !entry.1.iter().any(|(existing_ix, existing_name_id)| {
                    *existing_ix == ix_u32 && *existing_name_id == sym_name_id
                }) {
                    entry.1.push((ix_u32, sym_name_id));
                }
            }
        }

        // ref_hit のクリア（次ファイル向け）
        for v in self.ref_hit.iter_mut() {
            *v = false;
        }
    }

    /// visit に失敗したファイルでも buffer だけは空にして次ファイルに備える。
    pub(super) fn reset_buffers(self) {
        self.ref_events.clear();
        self.def_events.clear();
        for v in self.ref_hit.iter_mut() {
            *v = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(sym_ix: u32, confidence: u8) -> RefEventMini {
        RefEventMini {
            sym_ix,
            line: 0,
            column: 0,
            caller_name_id: 0,
            is_import: false,
            confidence,
        }
    }

    /// generic method name 判定: 仕様の全名・大文字混在・該当外の確認
    #[test]
    fn generic_method_name_recognizes_all_keywords() {
        for name in [
            "new", "update", "save", "delete", "get", "set", "find", "create", "make", "build",
            "init", "add", "remove",
        ] {
            assert!(
                is_generic_method_name(name),
                "{name} は generic 判定されるべき"
            );
        }
        // PHP は基本 case-sensitive だが、防御的に ASCII 大文字混在も拾う。
        assert!(is_generic_method_name("New"));
        assert!(is_generic_method_name("UPDATE"));
        // 該当外
        assert!(!is_generic_method_name("doSomethingSpecific"));
        assert!(!is_generic_method_name(""));
    }

    /// `BareNameOnly` + generic name のみが low_confidence 振り分け対象になる
    #[test]
    fn low_confidence_routing_requires_bare_and_generic_name() {
        let names = vec!["new".to_string(), "doSpecific".to_string()];

        // BareNameOnly + generic → 振り分け対象
        assert!(is_low_confidence_caller(&ev(0, 2), &names));
        // BareNameOnly だが non-generic → 対象外 (シンボル名で十分一意)
        assert!(!is_low_confidence_caller(&ev(1, 2), &names));
        // ExactOwner / InferredOwner は generic でも振り分けない (受信側が確定)
        assert!(!is_low_confidence_caller(&ev(0, 0), &names));
        assert!(!is_low_confidence_caller(&ev(0, 1), &names));
        // sym_ix が範囲外 → false (panic させない)
        assert!(!is_low_confidence_caller(&ev(99, 2), &names));
    }
}
